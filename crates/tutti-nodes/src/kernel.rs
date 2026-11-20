//! [`Kernel`]: what an effect tells the one node that runs it, and
//! [`KernelNode`], that node.
//!
//! A kernel is the effect's algorithm with its state: it designs itself from its plain
//! controls ([`Kernel::retarget`]) and runs a stretch of audio
//! ([`Kernel::render`]). Everything about being a graph node is the node's, written
//! once: reading the param cells and ports, the solve grid on the stream clock, the
//! corners of a driving port, the dead-band, the key bus, the fork.

use core::ops::Range;

use tutti_buffers::MaxBlock;
use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_graph::{
    BusRole, Cx, HasParam, HasParams, InBus, IntoNode, Io, MAX_PORTS, Node, NodeParts, Offset,
    OutBus, ParamNode, ParamSet, Prepare, RtLevel, Shape, Status,
};
use tutti_units::{Bpm, Latency, Real, SampleRate, Samples, Tail};

use crate::params::{At, ControlSet, HasControl, HasControls, Params};
use crate::tail_clock::{Heard, TailClock, tail_frames};

/// The most per-frame control signals a [`ControlSignals`] carries.
///
/// Four serve every effect that reads control signals (a limiter's threshold and ceiling are two).
pub const MAX_SIGNALS: usize = 4;

/// The per-frame control signals of a block's [`ControlRead::Fast`](crate::ControlRead::Fast)
/// controls, in the order of their rows: the frames a port carries, in the port's own unit
/// and the graph's float `F`, each `None` where nothing drives it.
///
/// Every control signal is as long as the frames the call renders.
#[derive(Clone, Copy, Debug, Default)]
pub struct ControlSignals<'a, F: Float>([Option<Mono<'a, F>>; MAX_SIGNALS]);

impl<'a, F: Float> ControlSignals<'a, F> {
    /// No control signal is driven.
    pub const NONE: Self = Self([None; MAX_SIGNALS]);

    /// Signal `k`, or `None` when nothing drives it, and for a `k` past [`MAX_SIGNALS`].
    #[must_use]
    pub fn get(&self, k: usize) -> Option<Mono<'a, F>> {
        self.0.get(k).copied().flatten()
    }
}

/// An effect's algorithm with its state, as [`KernelNode`] runs it.
///
/// Built on the control side, where it allocates everything it will hold; nothing it does
/// after that allocates.
pub trait Kernel: Clone + Send + 'static {
    /// The float of the kernel's state and of the audio it runs: the graph's, so a kernel of
    /// one float is a node of a graph of that float and no sample converts between them.
    type Float: Float;

    /// Its params: their addresses, units and the plain controls they assemble into.
    type Set: ControlSet;

    /// The frames between two designs while a control moves, at least one: the node
    /// designs at every `GRID`-th frame of the stream clock, counted from the last reset,
    /// and only while a control moves, so a held block renders in one call.
    const GRID: usize;

    /// Whether the node cuts at every grid point whether or not a control moved, and
    /// retargets there: for a kernel whose design moves every frame (a phaser's LFO). Then
    /// a render call never spans a grid point, and a retarget at a frame that is not a grid
    /// point is a control that moved. A kernel that does not move on its own leaves it
    /// `false`.
    const CUT_ALWAYS: bool = false;

    /// The ports: the input and output buses and the cost. The node adds the params, the
    /// latency and the tail.
    fn shape(&self) -> Shape;

    /// Prepares for `rate` and blocks of at most `max_block` frames. Control side. The
    /// first [`retarget`](Self::retarget) after it lands at once.
    fn prepare(&mut self, rate: SampleRate, max_block: MaxBlock);

    /// Back to silence. Audio thread: no allocation.
    fn reset(&mut self);

    /// The tempo of the block's first frame, as the transport says it
    /// ([`Cx::transport_at`]): called once at the start of each block, before the block's
    /// controls are read. A stopped transport has its set tempo. The default ignores it.
    /// Audio thread: no allocation.
    fn tempo(&mut self, tempo: Bpm) {
        let _ = tempo;
    }

    /// Designs `controls` and sets the running coefficients' target to the design. Audio thread:
    /// no allocation.
    fn retarget(&mut self, controls: &<Self::Set as ControlSet>::Controls);

    /// Runs `input` into `output`, the same number of frames, with the controls a port
    /// drives per frame in `signals`. Audio thread: no allocation.
    fn render(
        &mut self,
        input: Planes<'_, Self::Float>,
        signals: ControlSignals<'_, Self::Float>,
        output: PlanesMut<'_, '_, Self::Float>,
    );

    /// Runs `input` into `output` as [`render`](Self::render) does, with the signal a
    /// detector listens to in `key`: the input bus of the kernel's [`shape`](Self::shape) with
    /// the [`Sidechain`](BusRole::Sidechain) role (a bus nobody wired is silence). The node
    /// calls this instead of `render` when the shape declares one, so a kernel with one
    /// overrides it; a block whose key bus (or main input bus) has no channels renders as silence.
    /// Audio thread: no allocation.
    fn render_keyed(
        &mut self,
        input: Planes<'_, Self::Float>,
        key: Planes<'_, Self::Float>,
        signals: ControlSignals<'_, Self::Float>,
        output: PlanesMut<'_, '_, Self::Float>,
    ) {
        let _ = key;
        self.render(input, signals, output);
    }

    /// The processing latency, once prepared.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// How long it rings after its input stops, once prepared: the most it can, over every
    /// control. A finite or unbounded tail is a ceiling the node enforces (see
    /// [`KernelNode`]); [`Tail::Unknown`] and [`Tail::None`] are only declared.
    fn tail(&self) -> Tail;

    /// How long it rings at `controls`, at most [`tail`](Self::tail), erring long: what the
    /// node narrows its tail to. A kernel whose ring-out does not follow its controls leaves
    /// the ceiling. It may design into the kernel's own scratch, and must not change the
    /// audio state: lines, ramps and phase are as they were. Audio thread: no allocation.
    fn tail_of(&mut self, _controls: &<Self::Set as ControlSet>::Controls) -> Tail {
        self.tail()
    }
}

/// An effect of any [`Kernel`] as a graph node of the kernel's float ([`Kernel::Float`]).
///
/// # Controls
///
/// Per block the node reads its cells once ([`Params`]) at the block's last frame. When
/// a port drives a control, or a cell moved past the dead-band, the block is cut at the
/// kernel's grid points ([`Kernel::GRID`], counted on the stream clock from the last reset,
/// so the render does not depend on where blocks fall) and at the corners of a driving
/// port, and each cut that moved a control designs and retargets before the frames after
/// it render. Otherwise the block renders in one call. A kernel that cuts at every grid
/// point ([`Kernel::CUT_ALWAYS`]) is cut and retargeted at each of them whether or not a
/// control moved.
///
/// # The tail
///
/// A kernel that declares a finite or unbounded [`tail`](Kernel::tail) rings for at most
/// that long, and the node holds it to it. Each block it narrows the declared ceiling to
/// what the controls in force ring ([`Kernel::tail_of`], the longest of what a driven port
/// takes in the block), held at the longer figure until a shorter decay has been heard
/// out, and reports it ([`Io::report_tail`]). Once the input has been silent for the
/// reported tail plus the latency, the node cuts the output off, resets the kernel and parks
/// ([`Status::Idle`]): what rings is below the kernel's floor, and a parked node is a fresh
/// one when it wakes (its stream clock restarts with it).
///
/// # Non-finite audio
///
/// A kernel never latches a NaN or an infinity. A block whose output holds one is passed on
/// as rendered, and the kernel is reset after it, so the next block starts from silence: one
/// non-finite input sample is heard in at most one block. A kernel fed non-finite input for
/// longer renders each block from it sample for sample, and resets after each.
///
/// # In a graph
///
/// Inserted with its [`ParamSet`] as its controls ([`tutti_graph::param_parts`]); a fork
/// shares nothing and starts silent.
pub struct KernelNode<K: Kernel> {
    kernel: K,
    params: Params<K::Set>,
    /// The output bus.
    out: OutBus,
    /// The input width.
    inputs: usize,
    /// The side-chain key bus: the input bus the kernel's shape gives the
    /// [`Sidechain`](BusRole::Sidechain) role, if any.
    key: Option<InBus>,
    /// What [`prepare`](Node::prepare) sets: `None` until the node has a rate.
    tuned: Option<Tuned<<K::Set as ControlSet>::Controls>>,
}

/// A [`KernelNode`]'s state that exists once it has a rate.
#[derive(Clone, Copy, Debug)]
struct Tuned<C> {
    rate: SampleRate,
    /// The controls the kernel was last designed from.
    applied: Option<C>,
    /// Frames rendered since the last reset: where the grid points fall.
    position: u64,
    /// The ring-out owed, or `None` when the kernel's tail is only declared and the node
    /// neither narrows nor parks.
    clock: Option<TailClock>,
    /// The tail of the controls last designed (read at a block's last frame), in frames.
    current: u64,
    /// Whether the kernel has rendered since its last reset: what a park must undo.
    dirty: bool,
}

impl<K: Kernel> std::fmt::Debug for KernelNode<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("KernelNode");
        d.field("inputs", &self.inputs);
        if let Some(t) = &self.tuned {
            d.field("rate", &t.rate).field("position", &t.position);
        }
        d.finish_non_exhaustive()
    }
}

/// What a [`KernelNode`] is made from: a [`Kernel`], the [`ControlSet`] that addresses its
/// params and the controls it starts at.
///
/// The way a node author outside this crate builds a node from a kernel of their own:
/// `KernelNode::from(config)`. The width is the kernel's: its [`shape`](Kernel::shape)
/// declares the buses.
///
/// ```
/// use tutti_buffers::{BusTopology, MaxBlock, Planes, PlanesMut};
/// use tutti_graph::{Buses, Shape};
/// use tutti_nodes::{ControlSet, ControlSignals, Kernel, KernelConfig, KernelNode};
/// use tutti_units::{SampleRate, Tail, UnitAddr};
///
/// /// A kernel with no params.
/// #[derive(Clone, Debug)]
/// struct NoParams;
///
/// impl ControlSet for NoParams {
///     type Controls = ();
///     fn addrs(&self) -> &'static [UnitAddr] {
///         &[]
///     }
///     fn defaults(&self) {}
///     fn flatten(&self, _: &(), _: &mut [f64]) {}
///     fn assemble(&self, _: &[f64]) {}
/// }
///
/// /// A mono pass-through.
/// #[derive(Clone)]
/// struct Wire;
///
/// impl Kernel for Wire {
///     type Float = f32;
///     type Set = NoParams;
///     const GRID: usize = 16;
///     fn shape(&self) -> Shape {
///         Shape::buses(Buses::main(BusTopology::MONO), Buses::main(BusTopology::MONO))
///     }
///     fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}
///     fn reset(&mut self) {}
///     fn retarget(&mut self, _: &()) {}
///     fn render(
///         &mut self,
///         input: Planes<'_, f32>,
///         _: ControlSignals<'_, f32>,
///         mut output: PlanesMut<'_, '_, f32>,
///     ) {
///         output
///             .plane_mut(0)
///             .samples_mut()
///             .copy_from_slice(input.plane(0).samples());
///     }
///     fn tail(&self) -> Tail {
///         Tail::None
///     }
/// }
///
/// let node = KernelNode::from(KernelConfig::new(Wire, NoParams));
/// # let _ = node;
/// ```
#[derive(Clone)]
pub struct KernelConfig<K: Kernel> {
    kernel: K,
    set: K::Set,
    controls: <K::Set as ControlSet>::Controls,
}

impl<K: Kernel> std::fmt::Debug for KernelConfig<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelConfig").finish_non_exhaustive()
    }
}

impl<K: Kernel> KernelConfig<K> {
    /// A kernel with its `set`, the controls at the set's [`defaults`](ControlSet::defaults).
    #[must_use]
    pub fn new(kernel: K, set: K::Set) -> Self {
        let controls = set.defaults();
        Self {
            kernel,
            set,
            controls,
        }
    }

    /// The controls the node starts at.
    #[must_use]
    pub fn controls(mut self, controls: <K::Set as ControlSet>::Controls) -> Self {
        self.controls = controls;
        self
    }
}

/// The node of `config`. It has no rate until the graph's `prepare` gives it one and renders
/// silence before. Allocates: build it before the node goes live.
///
/// # Panics
///
/// If the kernel's [`shape`](Kernel::shape) declares no output bus.
impl<K: Kernel> From<KernelConfig<K>> for KernelNode<K> {
    fn from(config: KernelConfig<K>) -> Self {
        Self::with_kernel(config.kernel, config.set, config.controls)
    }
}

impl<K: Kernel> KernelNode<K> {
    /// A node running `kernel`, its cells `set` at `controls`. It has no rate until the
    /// graph's `prepare` gives it one and renders silence before. Allocates: build it
    /// before the node goes live.
    ///
    /// # Panics
    ///
    /// If the kernel's [`shape`](Kernel::shape) declares no output bus.
    #[must_use]
    pub(crate) fn with_kernel(
        kernel: K,
        set: K::Set,
        controls: <K::Set as ControlSet>::Controls,
    ) -> Self {
        const { assert!(K::GRID > 0, "a kernel's grid is at least one frame") };
        let shape = kernel.shape();
        let out = shape.output_bus(0).expect("a kernel has an output");
        let inputs = shape.input_bus(0).map_or(0, |b| b.width());
        let key = shape
            .buses_in()
            .iter()
            .position(|b| b.role == BusRole::Sidechain)
            .and_then(|i| shape.input_bus(i));
        let params = Params::new(set, &controls);
        Self {
            kernel,
            params,
            out,
            inputs,
            key,
            tuned: None,
        }
    }

    /// Back to silence: the grid counted from here, no ring-out owed, no input heard.
    fn clear(&mut self) {
        self.kernel.reset();
        if let Some(t) = &mut self.tuned {
            t.position = 0;
            t.dirty = false;
            if let Some(clock) = &mut t.clock {
                clock.clear();
            }
        }
        self.params.land();
    }

    /// How many frames of the block may still sound, for a kernel that declares a finite or
    /// unbounded tail; reports the tail it owes. `target` is the controls at the block's last
    /// frame, `moved` whether they differ from the last design.
    fn cut(
        &mut self,
        io: &mut Io<'_, K::Float>,
        target: &<K::Set as ControlSet>::Controls,
        moved: bool,
    ) -> usize {
        let size = io.frames();
        let inputs = self.inputs;
        let Some(t) = &mut self.tuned else {
            return size;
        };
        let Some(clock) = &mut t.clock else {
            return size;
        };
        if moved {
            t.current = tail_frames(self.kernel.tail_of(target));
        }
        let mut current = t.current;
        if let Some(longest) = self.params.longest(io)
            && Some(longest) != t.applied
        {
            current = current.max(tail_frames(self.kernel.tail_of(&longest)));
        }
        let last_input = (0..inputs)
            .filter_map(|c| {
                io.input(c)[..size]
                    .iter()
                    .rposition(|&s| s != K::Float::ZERO)
            })
            .max();
        let owed = clock.advance(current, Heard { size, last_input });
        io.report_tail(owed.tail(t.rate));
        owed.live
    }

    /// Renders `frames` of the block through the kernel.
    fn render(&mut self, io: &mut Io<'_, K::Float>, frames: Range<usize>) {
        if frames.is_empty() {
            return;
        }
        if let Some(t) = &mut self.tuned {
            t.dirty = true;
        }
        let driven = self.params.signals(io);
        let (ins, mut outs) = io.split();
        let (input, _) = slice_in((0..self.inputs).map(|c| ins.get(c)), &frames);
        let (key, keys) = match self.key {
            Some(bus) => slice_in(ins.bus(bus).channels(), &frames),
            None => ([&[][..]; MAX_PORTS], 0),
        };
        let (mut output, width) = slice_out(outs.bus(self.out).into_channels(), &frames);
        // Both views are whole planes of `frames.len()` frames, at most `MAX_PORTS` wide
        // (the most ports a shape declares, so a crossover's bands are all written). A bus
        // can be declared with no channels (an empty `BusTopology`): an output with none has
        // nothing to write, and an input or key bus with none has nothing to render or detect,
        // so the output renders as silence rather than as the arena had it.
        let Ok(output) = PlanesMut::new(&mut output[..width]) else {
            return;
        };
        let silence = |mut output: PlanesMut<'_, '_, K::Float>| {
            for c in 0..output.channels() {
                output.plane_mut(c).samples_mut().fill(K::Float::ZERO);
            }
        };
        let Ok(input) = Planes::new(&input[..self.inputs]) else {
            return silence(output);
        };
        let mut signals = ControlSignals::<K::Float>::NONE;
        for (slot, signal) in signals.0.iter_mut().zip(driven) {
            *slot = signal.map(|l| Mono::new(&l[frames.clone()]));
        }
        match self.key {
            None => self.kernel.render(input, signals, output),
            Some(_) => match Planes::new(&key[..keys]) {
                Ok(key) => self.kernel.render_keyed(input, key, signals, output),
                Err(_) => silence(output),
            },
        }
    }

    /// Renders the block cut at the grid points and the corners of a driving port, the
    /// block beginning at stream frame `position`: each cut reads the controls at its
    /// frame, and one that moved past the dead-band of the last design designs and
    /// retargets before the frames after it render. Cuts that did not move render
    /// together, in one call.
    fn render_steps(&mut self, io: &mut Io<'_, K::Float>, position: u64, rate: SampleRate) {
        let size = io.frames();
        walk_grid(
            &mut (self, io),
            Grid {
                position,
                size,
                period: K::GRID,
            },
            |(node, io), from| node.params.corner_from(io, from),
            |(node, io), frame, at_grid| {
                let target = node.params.read(
                    io,
                    At {
                        position,
                        frame,
                        rate,
                    },
                );
                let applied = node.tuned.as_ref().and_then(|t| t.applied);
                let moved = applied.is_none_or(|a| node.params.moved(&a, &target));
                ((K::CUT_ALWAYS && at_grid) || moved).then_some(target)
            },
            |(node, _), target| node.apply(target),
            |(node, io), frames| node.render(io, frames),
        );
    }

    /// Parks: the kernel silent and the grid counted from here, as a fresh node, unless
    /// nothing has rendered since it last was.
    fn park(&mut self) {
        if self.tuned.as_ref().is_some_and(|t| t.dirty) {
            self.clear();
        } else if let Some(t) = &mut self.tuned {
            t.position = 0;
            self.params.land();
        }
    }

    /// The first NaN or infinity in the block's output, as `(frame, channel)`.
    fn first_non_finite_output(&self, io: &mut Io<'_, K::Float>) -> Option<(usize, usize)> {
        let (_, mut outs) = io.split();
        first_non_finite(outs.bus(self.out).into_channels())
    }

    fn apply(&mut self, controls: <K::Set as ControlSet>::Controls) {
        self.kernel.retarget(&controls);
        if let Some(t) = &mut self.tuned {
            t.applied = Some(controls);
        }
    }
}

/// The earliest NaN or infinity across `channels` as `(frame, channel)`, the lowest channel
/// on a tie, in one pass: a channel is scanned only up to the best frame found so far.
pub(crate) fn first_non_finite<G: Float, T: AsRef<[G]>>(
    channels: impl Iterator<Item = T>,
) -> Option<(usize, usize)> {
    let mut first: Option<(usize, usize)> = None;
    for (channel, ch) in channels.enumerate() {
        let ch = ch.as_ref();
        let end = first.map_or(ch.len(), |(frame, _)| frame.min(ch.len()));
        if let Some(frame) = ch[..end].iter().position(|s| !s.is_finite()) {
            first = Some((frame, channel));
        }
    }
    first
}

/// Logs the reset that follows a non-finite output at `frame` of `channel`.
pub(crate) fn log_non_finite<G: Float>(io: &Io<'_, G>, frame: usize, channel: usize) {
    // `frame` is inside the block (both callers scan channels `io.frames()` long), so the
    // offset exists; a frame outside it would log nothing rather than a wrong offset.
    if let Some(at) = io.offset(frame) {
        io.log(
            RtLevel::Warn,
            "non-finite output; the node's state resets",
            at,
            &[("channel", channel.into())],
        );
    }
}

/// The first [`MAX_PORTS`] of `channels`, each cut to `frames`, and how many there are.
pub(crate) fn slice_in<'a, G: Float>(
    channels: impl Iterator<Item = &'a [G]>,
    frames: &Range<usize>,
) -> ([&'a [G]; MAX_PORTS], usize) {
    let mut planes: [&[G]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut n = 0;
    for (slot, ch) in planes.iter_mut().zip(channels) {
        *slot = &ch[frames.clone()];
        n += 1;
    }
    (planes, n)
}

/// [`slice_in`] for channels written to.
pub(crate) fn slice_out<'a, G: Float>(
    channels: impl Iterator<Item = &'a mut [G]>,
    frames: &Range<usize>,
) -> ([&'a mut [G]; MAX_PORTS], usize) {
    let mut planes: [&mut [G]; MAX_PORTS] = core::array::from_fn(|_| Default::default());
    let mut n = 0;
    for (slot, ch) in planes.iter_mut().zip(channels) {
        *slot = &mut ch[frames.clone()];
        n += 1;
    }
    (planes, n)
}

/// Runs `f` over the main input and output buses of a stateless through node, each cut to
/// the whole block: the views of `inputs` channels in and `outputs` out. A bus with no
/// channels has nothing to render.
pub(crate) fn through_planes<G: Float>(
    io: &mut Io<'_, G>,
    inputs: usize,
    outputs: usize,
    f: impl FnOnce(Planes<'_, G>, PlanesMut<'_, '_, G>),
) {
    debug_assert_eq!(
        inputs, outputs,
        "a through node writes every channel it reads"
    );
    let frames = 0..io.frames();
    let (ins, mut outs) = io.split();
    let (input, _) = slice_in((0..inputs).map(|c| ins.get(c)), &frames);
    let (mut output, _) = slice_out(outs.iter_mut().take(outputs), &frames);
    let (n_in, n_out) = (inputs.min(MAX_PORTS), outputs.min(MAX_PORTS));
    if let Ok(input) = Planes::new(&input[..n_in])
        && let Ok(output) = PlanesMut::new(&mut output[..n_out])
    {
        f(input, output);
        return;
    }
    // No view to render (a bus of no channels): every channel is silence, none is left as the
    // arena had it.
    for ch in &mut output[..n_out] {
        ch.fill(G::ZERO);
    }
}

/// Where a block falls on the stream's grid: it has `size` frames, begins at stream frame
/// `position` (frames since the last reset), and the grid has a point every `period` frames
/// from frame 0.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Grid {
    pub(crate) position: u64,
    pub(crate) size: usize,
    pub(crate) period: usize,
}

/// Walks a block over the grid and the corners of the controls: the one cut loop of a node
/// whose design moves on the stream clock.
///
/// At each cut (a grid point, or an earlier corner), `moved` reads the controls at that
/// frame and returns the design to take if they moved; the frames held so far are
/// `render`ed first, then `apply` takes the design and the held span restarts at the cut.
/// The frames after the last cut are `render`ed. `corner_from(state, from)` is the first
/// corner at or after frame `from`, if any.
pub(crate) fn walk_grid<S, T>(
    state: &mut S,
    grid: Grid,
    corner_from: impl Fn(&mut S, usize) -> Option<usize>,
    mut moved: impl FnMut(&mut S, usize, bool) -> Option<T>,
    mut apply: impl FnMut(&mut S, T),
    mut render: impl FnMut(&mut S, Range<usize>),
) {
    let Grid {
        position,
        size,
        period,
    } = grid;
    let first = ((period as u64 - position % period as u64) % period as u64) as usize;
    let mut grid = (first..size).step_by(period).peekable();
    let mut held = 0;
    let mut from = 0;
    loop {
        let corner = corner_from(state, from);
        let (cut, at_grid) = match (grid.peek().copied(), corner) {
            (Some(g), Some(c)) if g <= c => {
                grid.next();
                (g, true)
            }
            (_, Some(c)) => (c, false),
            (Some(g), None) => {
                grid.next();
                (g, true)
            }
            (None, None) => break,
        };
        from = cut + 1;
        if let Some(target) = moved(state, cut, at_grid) {
            render(state, held..cut);
            apply(state, target);
            held = cut;
        }
    }
    render(state, held..size);
}

impl<K: Kernel> Node<K::Float> for KernelNode<K> {
    /// The ports and cells always; latency and tail from the prepared rate. Unprepared, a
    /// node declares no latency and an unknown tail.
    fn shape(&self) -> Shape {
        let shape = self.kernel.shape().with_param_addrs(self.params.addrs());
        if self.tuned.is_none() {
            return shape.with_tail(Tail::Unknown);
        }
        shape
            .with_latency(Latency::new(self.kernel.latency()))
            .with_tail(self.kernel.tail())
    }

    fn prepare(&mut self, p: &Prepare) {
        self.kernel.prepare(p.sample_rate(), p.max_block());
        self.params.retune(p.sample_rate(), true);
        let latency = self.kernel.latency().get() as u64;
        self.tuned = Some(Tuned {
            rate: p.sample_rate(),
            applied: None,
            position: 0,
            clock: TailClock::of(self.kernel.tail(), latency),
            current: 0,
            dirty: false,
        });
        self.clear();
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, K::Float>) -> Status {
        let Some(t) = &mut self.tuned else {
            return Status::Silent;
        };
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let (position, rate, applied) = (t.position, t.rate, t.applied);
        t.position += size as u64;

        self.kernel.tempo(cx.transport_at(Offset::ZERO).tempo);

        self.params.begin(position);
        let target = self.params.read(
            &io,
            At {
                position,
                frame: size - 1,
                rate,
            },
        );
        let moved = applied
            .as_ref()
            .is_none_or(|a| self.params.moved(a, &target));
        let stepped = K::CUT_ALWAYS || self.params.driven(&io) || (applied.is_some() && moved);

        // Past its tail the kernel is cut off and parked: what rings is below its floor, and
        // the input has been silent for longer than the latency, so the output is silent.
        let live = self.cut(&mut io, &target, moved);
        if live == 0 {
            self.park();
            return Status::Idle;
        }
        if stepped {
            self.render_steps(&mut io, position, rate);
        } else {
            if moved {
                self.apply(target);
            }
            self.render(&mut io, 0..size);
        }
        if live < size {
            let (_, mut outs) = io.split();
            for ch in outs.bus(self.out).into_channels() {
                ch[live..size].fill(K::Float::ZERO);
            }
            self.park();
        } else if let Some((frame, channel)) = self.first_non_finite_output(&mut io) {
            log_non_finite(&io, frame, channel);
            self.clear();
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.params.base(k)
    }
}

/// A clone shares the cells and starts silent.
impl<K: Kernel> Clone for KernelNode<K> {
    fn clone(&self) -> Self {
        let mut kernel = self.kernel.clone();
        kernel.reset();
        let mut params = self.params.clone();
        params.land();
        Self {
            kernel,
            params,
            out: self.out,
            inputs: self.inputs,
            key: self.key,
            tuned: self.tuned.map(|t| Tuned {
                position: 0,
                clock: t.clock.map(|mut clock| {
                    clock.clear();
                    clock
                }),
                dirty: false,
                ..t
            }),
        }
    }
}

impl<K: Kernel> ParamNode<K::Float> for KernelNode<K> {
    fn param_set(&self) -> ParamSet {
        self.params.bind(ParamSet::builder()).build()
    }

    /// A clone with every cell detached (at its value now) and silent.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.params.detach();
        fork
    }
}

/// A kernel node has the params its [`ControlSet`] declares, in the units its controls
/// hold them in.
impl<K: Kernel, const P: u16> HasParam<P> for KernelNode<K>
where
    K::Set: HasControl<P>,
{
    type Unit = <K::Set as HasControl<P>>::Unit;
}

impl<K: Kernel, const P: u16> HasParams<P> for KernelNode<K> where K::Set: HasControls<P> {}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl<K: Kernel> IntoNode<K::Float> for KernelNode<K> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, K::Float> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod latency_tail;

#[cfg(test)]
mod tests {
    use tutti_buffers::BusTopology;
    use tutti_dsp::{DelayControls, DelayLayout, InterpolationMode, MaxDelay};
    use tutti_graph::contract::{drive_input, prepared};
    use tutti_graph::{Env, PlayClock, Status, TransportChanges, TransportPosition};
    use tutti_units::{Frame, Seconds};

    use super::*;
    use crate::{DelayConfig, DelayNode};

    /// The earliest frame wins across channels, the lowest channel on a tie, and channels of
    /// unequal length are each read to their own end.
    #[test]
    fn first_non_finite_is_the_earliest_frame_then_the_lowest_channel() {
        let n = f32::NAN;
        let clean: [&[f32]; 2] = [&[0.0; 4], &[1.0; 4]];
        assert_eq!(first_non_finite(clean.into_iter()), None);
        let later_channel_earlier_frame: [&[f32]; 3] = [
            &[0.0, 0.0, 0.0, n],
            &[0.0, n, 0.0, 0.0],
            &[0.0, 0.0, n, 0.0],
        ];
        assert_eq!(
            first_non_finite(later_channel_earlier_frame.into_iter()),
            Some((1, 1))
        );
        let tie: [&[f32]; 3] = [&[0.0, 0.0, 0.0], &[0.0, 0.0, f32::INFINITY], &[0.0, 0.0, n]];
        assert_eq!(first_non_finite(tie.into_iter()), Some((2, 1)));
        let unequal: [&[f32]; 2] = [&[0.0, 0.0, 0.0, 0.0, 0.0, n], &[0.0, 0.0, n]];
        assert_eq!(first_non_finite(unequal.into_iter()), Some((2, 1)));
        let short_first: [&[f32]; 2] = [&[n], &[n, n, n]];
        assert_eq!(first_non_finite(short_first.into_iter()), Some((0, 0)));
    }

    /// A kernel whose tail is only declared is left alone: a delay (tail unknown) that has
    /// heard one impulse and then nothing for 4 000 blocks is never parked, never cut and
    /// never narrows its tail, and keeps its echoes.
    #[test]
    fn a_kernel_with_an_unknown_tail_is_never_narrowed_cut_or_parked() {
        let rate = SampleRate::SR_48K;
        let mut node = prepared(
            DelayNode::<f32>::from(
                DelayConfig::new(DelayLayout::new(
                    BusTopology::MONO,
                    MaxDelay::of(Seconds::new(0.01)),
                    InterpolationMode::Linear,
                ))
                .controls(DelayControls {
                    feedback: tutti_units::Feedback::new(0.5),
                    ..DelayControls::DEFAULT
                }),
            ),
            rate,
            64,
        );
        let mut x = vec![0.0f32; 64];
        let mut heard = false;
        for block in 0..4_000u64 {
            let env = Env {
                frame: Frame::new(block * 64),
                sample_rate: rate,
                block_len: Samples::new(64),
                transport: TransportPosition::default(),
                changes: TransportChanges::NONE,
                play: PlayClock::ZERO,
            };
            x[0] = if block == 0 { 1.0 } else { 0.0 };
            let d = drive_input(&mut node, &env, Planes::new(&[&x]).unwrap(), &[], &[]);
            assert_eq!(d.status, Some(Status::Modified), "block {block}");
            assert_eq!(d.reported_tail, None, "block {block}");
            heard |= d.audio[0].iter().any(|&s| s != 0.0);
        }
        assert!(heard);
    }

    /// A kernel with a sidechain bus or a main input bus of no channels (either can be
    /// declared: an empty topology) renders silence, not what the output held: nothing is
    /// detected or passed. A zero-width output has nothing to write.
    #[test]
    fn a_bus_with_no_channels_renders_silence() {
        use tutti_buffers::BusTopology;
        use tutti_graph::contract::Direct;
        use tutti_graph::{Bus, BusRole, Buses};
        use tutti_units::Db;

        #[derive(Clone, Copy, Debug, PartialEq)]
        struct Gain {
            gain: Db,
        }
        #[derive(Clone, Copy, Debug)]
        struct GainSet;
        control_set! {
            GainSet(_s): Gain = Gain { gain: Db::new(0.0) };
            live: (all);
            GainDb => Db = gain { whole };
            fn defaults(&self) -> Gain {
                Gain { gain: Db::new(0.0) }
            }
        }
        /// Never asked to render: the node refuses each block it is declared empty for.
        #[derive(Clone)]
        struct Keyed {
            main: BusTopology,
            key: Option<BusTopology>,
        }
        impl Kernel for Keyed {
            type Float = f32;
            type Set = GainSet;
            const GRID: usize = 16;
            fn shape(&self) -> Shape {
                let mut main = Buses::main(self.main);
                if let Some(key) = self.key {
                    main = main
                        .with(Bus::new(key, BusRole::Sidechain))
                        .expect("two buses");
                }
                let out = Bus::banded(BusTopology::MONO, BusRole::Main, 1).expect("a bus");
                Shape::buses(main, Buses::NONE.with(out).expect("one bus"))
            }
            fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}
            fn reset(&mut self) {}
            fn retarget(&mut self, _: &Gain) {}
            fn render(
                &mut self,
                _: Planes<'_, f32>,
                _: ControlSignals<'_, f32>,
                _: PlanesMut<'_, '_, f32>,
            ) {
                unreachable!("an empty bus is refused before the kernel renders");
            }
            fn tail(&self) -> Tail {
                Tail::None
            }
        }
        for (main, key) in [
            (BusTopology::MONO, Some(BusTopology::EMPTY)),
            (BusTopology::EMPTY, None),
            (BusTopology::EMPTY, Some(BusTopology::MONO)),
        ] {
            let node =
                KernelNode::with_kernel(Keyed { main, key }, GainSet, Gain { gain: Db::new(0.0) });
            let mut d = Direct::new(node, SampleRate::SR_48K, 64);
            for c in d.inputs_mut().iter_mut() {
                let c = c.into_samples_mut();
                c.fill(0.5);
            }
            d.outputs_mut().plane_mut(0).samples_mut().fill(7.0);
            d.block();
            assert!(
                d.output().plane(0).iter().all(|&s| s == 0.0),
                "{main:?}, key {key:?}"
            );
        }
    }

    /// What a mono pass-through kernel hears of the tempo: the last and the count of the
    /// [`tempo`](Kernel::tempo) calls.
    #[derive(Clone, Debug, Default)]
    struct Heard {
        last: std::sync::Arc<std::sync::atomic::AtomicU64>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    /// A mono pass-through that records the tempo it is told.
    #[derive(Clone)]
    struct Tempo {
        heard: Heard,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct NoControls {
        gain: tutti_units::Db,
    }
    #[derive(Clone, Copy, Debug)]
    struct NoControlsSet;
    control_set! {
        NoControlsSet(_s): NoControls = NoControls { gain: tutti_units::Db::new(0.0) };
        live: (all);
        GainDb => tutti_units::Db = gain { whole };
        fn defaults(&self) -> NoControls {
            NoControls { gain: tutti_units::Db::new(0.0) }
        }
    }

    impl Kernel for Tempo {
        type Float = f32;
        type Set = NoControlsSet;
        const GRID: usize = 16;
        fn shape(&self) -> Shape {
            use tutti_graph::Buses;
            Shape::buses(
                Buses::main(BusTopology::MONO),
                Buses::main(BusTopology::MONO),
            )
        }
        fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}
        fn reset(&mut self) {}
        fn tempo(&mut self, tempo: Bpm) {
            use std::sync::atomic::Ordering::Relaxed;
            self.heard.last.store(tempo.get().to_bits(), Relaxed);
            self.heard.calls.fetch_add(1, Relaxed);
        }
        fn retarget(&mut self, _: &NoControls) {}
        fn render(
            &mut self,
            input: Planes<'_, f32>,
            _: ControlSignals<'_, f32>,
            mut output: PlanesMut<'_, '_, f32>,
        ) {
            output
                .plane_mut(0)
                .samples_mut()
                .copy_from_slice(input.plane(0).samples());
        }
        fn tail(&self) -> Tail {
            Tail::None
        }
    }

    fn tempo_node() -> (KernelNode<Tempo>, Heard) {
        let heard = Heard::default();
        let kernel = Tempo {
            heard: heard.clone(),
        };
        let node = KernelNode::with_kernel(
            kernel,
            NoControlsSet,
            NoControls {
                gain: tutti_units::Db::new(0.0),
            },
        );
        (prepared(node, SampleRate::SR_48K, 64), heard)
    }

    fn block_under(node: &mut KernelNode<Tempo>, transport: TransportPosition) {
        let env = Env {
            frame: Frame::new(0),
            sample_rate: SampleRate::SR_48K,
            block_len: Samples::new(64),
            transport,
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        drive_input(
            node,
            &env,
            Planes::new(&[&[0.0f32; 64][..]]).unwrap(),
            &[],
            &[],
        );
    }

    /// A kernel is told, once a block, the tempo of the transport the
    /// block runs under: 90 bpm playing, and a stopped transport's set 75 bpm (a stopped
    /// transport holds its position and its tempo).
    #[test]
    fn a_kernel_hears_the_transports_tempo_once_a_block() {
        use std::sync::atomic::Ordering::Relaxed;
        use tutti_units::Beat;
        let (mut node, heard) = tempo_node();
        block_under(
            &mut node,
            TransportPosition::new(true, Bpm::of(90.0), Beat::new(8.0), None),
        );
        assert_eq!(f64::from_bits(heard.last.load(Relaxed)), 90.0);
        assert_eq!(heard.calls.load(Relaxed), 1);
        block_under(
            &mut node,
            TransportPosition::new(false, Bpm::of(75.0), Beat::new(3.0), None),
        );
        assert_eq!(f64::from_bits(heard.last.load(Relaxed)), 75.0);
        assert_eq!(heard.calls.load(Relaxed), 2);
    }
}
