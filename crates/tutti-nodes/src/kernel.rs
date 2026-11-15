//! [`Kernel`]: what a family of effects tells the one node that runs it, and
//! [`KernelNode`], that node.
//!
//! A kernel is the effect's algorithm with its state: it designs itself from its plain
//! controls ([`Kernel::retarget`]) and runs a stretch of audio
//! ([`Kernel::render`]). Everything about being a graph node is the node's, written
//! once: reading the param cells and ports, the solve grid on the stream clock, the
//! corners of a driving port, the dead-band, the fork.

use core::ops::Range;

use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_dsp::MAX_LANES;
use tutti_graph::{
    Cx, HasParam, HasParams, IntoNode, Io, MAX_PORTS, Node, NodeParts, OutBus, ParamNode, ParamSet,
    Prepare, Shape, Status,
};
use tutti_types::{Latency, MaxBlock, SampleRate, Samples, Tail};

use crate::params::{At, ControlSet, HasControl, HasControls, Params};
use crate::tail_clock::{Heard, TailClock, tail_frames};

/// The per-frame lanes of a block's [`ControlRead::Fast`](crate::ControlRead::Fast)
/// controls, in the order of their rows: the frames a port carries, in the port's own unit
/// and the sample format `S` of the audio, each `None` where nothing drives it.
///
/// Every lane is as long as the frames the call renders.
#[derive(Clone, Copy, Debug, Default)]
pub struct ControlLanes<'a, S: Float = f32>([Option<Mono<'a, S>>; MAX_LANES]);

impl<'a, S: Float> ControlLanes<'a, S> {
    /// No lane is driven.
    pub const NONE: Self = Self([None; MAX_LANES]);

    /// Lane `k`, or `None` when nothing drives it, and for a `k` past [`MAX_LANES`].
    #[must_use]
    pub fn get(&self, k: usize) -> Option<Mono<'a, S>> {
        self.0.get(k).copied().flatten()
    }
}

/// An effect's algorithm with its state, as [`KernelNode`] runs it.
///
/// Built on the control side, where it allocates everything it will hold; nothing it does
/// after that allocates.
pub trait Kernel: Clone + Send + 'static {
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

    /// Designs `controls` and aims the running coefficients at the design. Audio thread:
    /// no allocation.
    fn retarget(&mut self, controls: &<Self::Set as ControlSet>::Controls);

    /// Runs `input` into `output`, the same number of frames, in any sample format (the
    /// graph's is `f32`), with the controls a port drives per frame in `lanes`. Audio
    /// thread: no allocation.
    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        lanes: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    );

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
    /// audio state: lines, glides and phase are as they were. Audio thread: no allocation.
    fn tail_of(&mut self, _controls: &<Self::Set as ControlSet>::Controls) -> Tail {
        self.tail()
    }
}

/// An effect of any [`Kernel`] as a graph node.
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

impl<K: Kernel> KernelNode<K> {
    /// A node running `kernel`, its cells `set` at `controls`. It has no rate until the
    /// graph's `prepare` gives it one and renders silence before. Allocates: build it
    /// before the node goes live.
    ///
    /// # Panics
    ///
    /// If the kernel's [`shape`](Kernel::shape) declares no output bus.
    #[must_use]
    pub fn with_kernel(kernel: K, set: K::Set, controls: <K::Set as ControlSet>::Controls) -> Self {
        const { assert!(K::GRID > 0, "a kernel's grid is at least one frame") };
        let shape = kernel.shape();
        let out = shape.output_bus(0).expect("a kernel has an output");
        let inputs = shape.input_bus(0).map_or(0, |b| b.width());
        let params = Params::new(set, &controls);
        Self {
            kernel,
            params,
            out,
            inputs,
            tuned: None,
        }
    }

    /// The controls the kernel was last designed from, for a node's own tests.
    #[cfg(test)]
    pub(crate) fn applied(&self) -> Option<<K::Set as ControlSet>::Controls> {
        self.tuned.as_ref().and_then(|t| t.applied)
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
        io: &mut Io<'_>,
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
            .filter_map(|c| io.input(c)[..size].iter().rposition(|&s| s != 0.0))
            .max();
        let owed = clock.advance(current, Heard { size, last_input });
        io.report_tail(owed.tail);
        owed.live
    }

    /// Renders `frames` of the block through the kernel.
    fn render(&mut self, io: &mut Io<'_>, frames: Range<usize>) {
        if frames.is_empty() {
            return;
        }
        if let Some(t) = &mut self.tuned {
            t.dirty = true;
        }
        let driven = self.params.lanes(io);
        let (ins, mut outs) = io.split();
        let mut input: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        for (c, slot) in input.iter_mut().enumerate().take(self.inputs) {
            *slot = &ins.get(c)[frames.clone()];
        }
        let mut output: [&mut [f32]; MAX_PORTS] = core::array::from_fn(|_| Default::default());
        let mut width = 0;
        for (slot, ch) in output.iter_mut().zip(outs.bus(self.out).into_channels()) {
            *slot = &mut ch[frames.clone()];
            width += 1;
        }
        // Both views are whole planes of `frames.len()` frames, at most `MAX_PORTS` wide
        // (the most ports a shape declares, so a crossover's bands are all written); a bus
        // with no channels is the one refusal, and it has nothing to render.
        if let (Ok(input), Ok(output)) = (
            Planes::new(&input[..self.inputs]),
            PlanesMut::new(&mut output[..width]),
        ) {
            let mut lanes = ControlLanes::<f32>::NONE;
            for (slot, lane) in lanes.0.iter_mut().zip(driven) {
                *slot = lane.map(|l| Mono::new(&l[frames.clone()]));
            }
            self.kernel.render(input, lanes, output);
        }
    }

    /// Renders the block cut at the grid points and the corners of a driving port, the
    /// block beginning at stream frame `position`: each cut reads the controls at its
    /// frame, and one that moved past the dead-band of the last design designs and
    /// retargets before the frames after it render. Cuts that did not move render
    /// together, in one call.
    fn render_steps(&mut self, io: &mut Io<'_>, position: u64, rate: SampleRate) {
        let size = io.frames();
        let period = K::GRID as u64;
        let first = ((period - position % period) % period) as usize;
        let mut grid = (first..size).step_by(K::GRID).peekable();
        let mut held = 0;
        let mut from = 0;
        loop {
            let corner = self.params.corner_from(io, from);
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
            let target = self.params.read(
                io,
                At {
                    position,
                    frame: cut,
                    rate,
                },
            );
            let applied = self.tuned.as_ref().and_then(|t| t.applied);
            if (K::CUT_ALWAYS && at_grid) || applied.is_none_or(|a| self.params.moved(&a, &target))
            {
                self.render(io, held..cut);
                self.apply(target);
                held = cut;
            }
        }
        self.render(io, held..size);
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

    fn apply(&mut self, controls: <K::Set as ControlSet>::Controls) {
        self.kernel.retarget(&controls);
        if let Some(t) = &mut self.tuned {
            t.applied = Some(controls);
        }
    }
}

impl<K: Kernel> Node for KernelNode<K> {
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

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let Some(t) = &mut self.tuned else {
            return Status::Silent;
        };
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let (position, rate, applied) = (t.position, t.rate, t.applied);
        t.position += size as u64;

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
                ch[live..size].fill(0.0);
            }
            self.park();
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
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

impl<K: Kernel> ParamNode for KernelNode<K> {
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
impl<K: Kernel> IntoNode for KernelNode<K> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use tutti_dsp::{DelayControls, MaxDelay};
    use tutti_graph::contract::{drive_input, prepared};
    use tutti_graph::{Env, PlayClock, Status, TransportChanges, TransportPosition};
    use tutti_types::{Frame, Seconds};

    use super::*;
    use crate::DelayNode;

    /// A kernel whose tail is only declared is left alone: a delay (tail unknown) that has
    /// heard one impulse and then nothing for 4 000 blocks is never parked, never cut and
    /// never narrows its tail, and keeps its echoes.
    #[test]
    fn a_kernel_with_an_unknown_tail_is_never_narrowed_cut_or_parked() {
        let rate = SampleRate::SR_48K;
        let mut node = prepared(
            DelayNode::mono(
                MaxDelay::of(Seconds(0.01)),
                DelayControls {
                    feedback: tutti_types::Feedback(0.5),
                    ..DelayControls::DEFAULT
                },
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
                block_len: Samples(64),
                transport: TransportPosition::default(),
                changes: TransportChanges::NONE,
                play: PlayClock::ZERO,
            };
            x[0] = if block == 0 { 1.0 } else { 0.0 };
            let d = drive_input(&mut node, &env, &[&x], &[], &[]);
            assert_eq!(d.status, Some(Status::Modified), "block {block}");
            assert_eq!(d.reported_tail, None, "block {block}");
            heard |= d.audio[0].iter().any(|&s| s != 0.0);
        }
        assert!(heard);
    }
}
