//! The node contract: [`Node`], and everything its four methods name.
//!
//! The contract is deliberately small:
//!
//! - **Latency is declared**, in [`Shape`], as a [`Latency`] — a type a
//!   musical delay time cannot be passed as, so a delay is never mistaken for
//!   processing latency by delay compensation.
//! - **Controls are typed at insertion.** A node's control surface comes back
//!   from [`IntoNode::into_node`] when it is inserted; nothing downcasts to
//!   find a node again.
//! - **Nothing clones a running unit**: the executor owns it, and a recompile
//!   keeps it by key.
//! - **One float.** Every buffer is planar in the graph's `F` (see the crate
//!   docs on precision).
//! - **Whole blocks.** A node gets whatever block the executor was prepared
//!   for, up to [`Prepare::max_block`], whole — sub-chunking at event offsets
//!   is the node's job, which keeps an out-of-process plugin's declared
//!   pipeline latency constant.

pub(crate) mod bus;
pub(crate) mod controls;
pub(crate) mod cost;
pub(crate) mod declare;
pub(crate) mod fork;
pub(crate) mod instrument;
pub(crate) mod io;
pub(crate) mod param;
pub(crate) mod param_event;
pub(crate) mod play;
pub(crate) mod rates;
pub(crate) mod render_mode;
pub(crate) mod shape;
pub(crate) mod time;
pub(crate) mod wake;

use std::sync::Arc;

use tutti_buffers::MaxBlock;

use tutti_music::MeterMap;
use tutti_tempo::TempoMap;
use tutti_units::{Latency, SampleRate, Samples, Tail};

use crate::exec::history::TransportHistory;
use crate::node::controls::ParamSet;
use crate::node::fork::ForkSource;
use crate::node::io::Io;
use crate::node::render_mode::RenderMode;
use crate::node::shape::Shape;
use crate::node::time::Offset;
use crate::node::time::{Env, TransportPosition};
use crate::node::wake::Wake;
use tutti_buffers::Float;
use tutti_rt::Reclaim;

use crate::exec::published::{ErasedPublished, HasPublished, Published, PublishedKind};
use tutti_units::PlayReading;

/// Most audio channels, or event ports, on one side of one node.
///
/// Bounded because the per-channel masks are `u64` bitsets and because the
/// executor builds each node's port table on the stack — no allocation per
/// node call. A node wider than this is a [`CompileError`](crate::CompileError),
/// not a silent truncation.
pub const MAX_PORTS: usize = 64;

/// Where a node writes its state for its successor ([`Node::chase`]): a
/// buffer the executor sized from [`Shape::chase`], of MIDI messages at
/// offset 0.
///
/// Writing past the declared capacity refuses the message, and the executor
/// counts every refusal; nothing grows.
#[derive(Debug)]
pub struct Chase<'a> {
    buf: &'a mut tutti_rt::CappedVec<crate::Event>,
    limit: usize,
    refused: u32,
}

impl<'a> Chase<'a> {
    /// A chase over `buf`, which takes `capacity` more messages.
    pub(crate) fn new(buf: &'a mut tutti_rt::CappedVec<crate::Event>, capacity: usize) -> Self {
        let limit = (buf.len() + capacity).min(buf.capacity());
        Self {
            buf,
            limit,
            refused: 0,
        }
    }

    /// Writes `ump` for the successor, after what was written. `false`, and
    /// counted, when the declared capacity is used up.
    pub fn push(&mut self, ump: tutti_ump::Ump) -> bool {
        if self.buf.len() >= self.limit {
            self.refused += 1;
            return false;
        }
        self.buf.push(crate::Event::midi(Offset::ZERO, ump));
        true
    }

    /// How many messages were refused.
    pub(crate) fn refused(&self) -> u32 {
        self.refused
    }
}

/// How finely a node honours event offsets: the timing it promises for what
/// arrives on its event inputs.
///
/// Ordered from finest to coarsest. A node written against
/// [`Io::sub_blocks`](crate::Io::sub_blocks) is [`Sample`](Self::Sample) by
/// construction; one bound by an engine that renders in fixed chunks declares
/// the chunk (a SoundFont, whose voices render 8 frames at a time, is `Frames(8)`); one that reads its
/// events once per call is [`Block`](Self::Block).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// Every event takes effect on its exact frame.
    Sample,
    /// Every event takes effect within `n - 1` frames of its exact frame,
    /// in either direction. No chunk grid is assumed, so a node
    /// that renders in `n`-frame chunks on its own cursor, whatever its
    /// phase against the blocks, honours it. `n` is at least 1; `Frames(1)`
    /// promises what `Sample` does.
    Frames(u32),
    /// Events take effect somewhere in the block they arrive in, whatever
    /// the offset: within one block of the exact frame. Also the resolution
    /// of a node with no event inputs, whose promise is vacuous.
    Block,
}

impl Resolution {
    /// Whether a node at this resolution satisfies an edge that requires
    /// `required`: it is at least as fine.
    ///
    /// `Frames(n)` satisfies `Frames(m)` when `n <= m` — a finer chunk is still
    /// inside the coarser one's promise — but never `Sample` unless `n == 1`.
    #[must_use]
    pub const fn satisfies(self, required: Resolution) -> bool {
        match (self, required) {
            (Resolution::Sample, _) | (_, Resolution::Block) => true,
            (Resolution::Frames(n), Resolution::Sample) => n <= 1,
            (Resolution::Frames(n), Resolution::Frames(m)) => n <= m,
            (Resolution::Block, _) => false,
        }
    }
}

/// What a node is prepared for. Handed to [`Node::prepare`] on the control
/// thread, before the node first processes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prepare {
    sample_rate: SampleRate,
    max_block: MaxBlock,
    quantum: Option<MaxBlock>,
    render_mode: RenderMode,
}

impl Prepare {
    /// Prepares for `sample_rate`, with blocks of up to `max_block` frames.
    #[must_use]
    pub const fn new(sample_rate: SampleRate, max_block: MaxBlock) -> Self {
        Self {
            sample_rate,
            max_block,
            quantum: None,
            render_mode: RenderMode::Realtime,
        }
    }

    /// This prepare, under `mode`: whether blocks arrive under realtime
    /// pressure. Realtime unless set.
    #[must_use]
    pub const fn with_render_mode(mut self, mode: RenderMode) -> Self {
        self.render_mode = mode;
        self
    }

    /// Whether blocks arrive under realtime pressure, fixed until the next
    /// prepare (never per block). See [`RenderMode`].
    pub const fn render_mode(&self) -> RenderMode {
        self.render_mode
    }

    /// Marks the current thread as inside a block ([`RtScope::enter`](tutti_rt::RtScope::enter)) when
    /// this prepare is [`Realtime`](RenderMode::Realtime), and not when it is
    /// [`Offline`](RenderMode::Offline): an offline render may block and
    /// allocate.
    pub(crate) fn enter_rt_scope(&self) -> Option<tutti_rt::RtScopeGuard> {
        (!self.render_mode.is_offline()).then(tutti_rt::RtScope::enter)
    }

    /// This prepare, for a host whose device calls back with `quantum`
    /// frames each time (the largest, if its callbacks vary).
    ///
    /// A hint about the host, not a bound on blocks: the executor may still
    /// hand a node any block up to [`max_block`](Self::max_block), cut however
    /// the host renders a callback. A node that must keep work in step with
    /// the device reads it — a hosted out-of-process plugin ships one
    /// callback's worth to its server per callback, so the server has a
    /// whole device period to answer. `None` (the default) when the host does
    /// not know, or renders offline.
    ///
    /// A quantum is a [`MaxBlock`]: the longest callback is the same quantity
    /// as the longest block (one frame to `u32::MAX`), so a zero quantum cannot
    /// be written. It is not required to be at most [`max_block`](Self::max_block):
    /// it is a hint about the host, not a bound on blocks, so there is no
    /// relation to refuse.
    #[must_use]
    pub const fn with_quantum(mut self, quantum: MaxBlock) -> Self {
        self.quantum = Some(quantum);
        self
    }

    /// The host's device callback size, when it knows one. See
    /// [`with_quantum`](Self::with_quantum).
    pub const fn quantum(&self) -> Option<MaxBlock> {
        self.quantum
    }

    /// The rate every block will run at.
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// The length of every declick fade at this rate: [`DECLICK`](crate::DECLICK)
    /// in frames. The one owner of the length of an edge's weight fade, a
    /// unit's mode fade and a param port's source fade.
    pub(crate) fn declick(&self) -> Samples {
        tutti_curve::declick_frames(self.sample_rate)
    }

    /// The longest block [`Node::process`] will be handed. Blocks may be any
    /// length from 1 up to this, and need not repeat.
    pub const fn max_block(&self) -> MaxBlock {
        self.max_block
    }
}

/// Planar per-channel scratch a node sizes in [`Node::prepare`], in the sample
/// format `F` of the [`Node`] it serves.
///
/// Built from the [`MaxBlock`] the node was prepared with
/// ([`Prepare::max_block`]), so every block the executor can hand the node fits:
/// [`channel`](Self::channel) takes the block's own length and never needs to be
/// clamped. A `MaxBlock` is a plain value anyone can build, so a scratch built
/// from another one is only as long as that, and a longer block panics.
#[derive(Clone, Debug, Default)]
pub struct Scratch<F: Float = f32> {
    data: Vec<F>,
    max: usize,
}

impl<F: Float> Scratch<F> {
    /// `channels` zeroed channels of `max` frames. Control thread: allocates.
    #[must_use]
    pub fn new(max: MaxBlock, channels: usize) -> Self {
        Self {
            data: vec![F::ZERO; max.get() * channels],
            max: max.get(),
        }
    }

    /// Channel `c`'s first `frames` samples.
    ///
    /// # Panics
    ///
    /// If `frames` exceeds the `MaxBlock` this was built from — which an
    /// executor-supplied block length never does.
    pub fn channel(&mut self, c: usize, frames: usize) -> &mut [F] {
        assert!(
            frames <= self.max,
            "{frames} frames past the prepared maximum {}",
            self.max
        );
        &mut self.data[c * self.max..c * self.max + frames]
    }
}

/// A 64-channel bitset: bit `c` is channel `c`. Channels past 63 are never set.
macro_rules! mask {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct $name(pub u64);

        impl $name {
            /// No channel set.
            pub const NONE: Self = Self(0);

            /// Whether channel `c` is set.
            #[inline]
            pub const fn get(self, c: usize) -> bool {
                c < 64 && (self.0 >> c) & 1 == 1
            }

            /// This mask with channel `c` set. A no-op past 63.
            #[inline]
            #[must_use]
            pub const fn with(self, c: usize) -> Self {
                if c < 64 {
                    Self(self.0 | (1 << c))
                } else {
                    self
                }
            }

            /// Every channel below `n` set.
            #[inline]
            pub const fn all(n: usize) -> Self {
                if n >= 64 {
                    Self(u64::MAX)
                } else {
                    Self((1u64 << n) - 1)
                }
            }

            /// Whether every channel below `n` is set.
            #[inline]
            pub const fn covers(self, n: usize) -> bool {
                let want = Self::all(n).0;
                self.0 & want == want
            }
        }
    };
}

mask!(
    /// Channels known to be **exact** silence (every sample `0.0`) for the
    /// whole block. Unset means unknown, not "not silent".
    SilenceMask
);
mask!(
    /// Channels known to hold one value for the whole block. Unset means
    /// unknown.
    ConstantMask
);
mask!(
    /// Channels the executor aliased in place: output `c` already holds input
    /// `c`. Only ever set for a node whose [`Shape::in_place`] is `true`.
    InPlaceMask
);
mask!(
    /// Audio ports with something on the other end: an input port with at
    /// least one source, an output port with at least one consumer (an edge,
    /// a feedback edge, a main output, a tap or a meter). Exact, never a
    /// hint: an unset input reads silence because nothing feeds it.
    WiredMask
);

/// Which of a node's audio ports are wired, from the compiled plan: the
/// plan's answer for the node, fixed between commits.
///
/// Port `p` is bit `p` of the side's mask, over every audio port of the
/// shape; [`Io::wired_inputs`](crate::Io::wired_inputs) and its kin read the
/// main bus and each bus by handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Wired {
    /// Input ports with a source.
    pub inputs: WiredMask,
    /// Output ports with a consumer.
    pub outputs: WiredMask,
}

impl Wired {
    /// Every one of `inputs` input ports and `outputs` output ports wired:
    /// what a hand-built [`Io`] reads.
    #[must_use]
    pub const fn all(inputs: usize, outputs: usize) -> Self {
        Self {
            inputs: WiredMask::all(inputs),
            outputs: WiredMask::all(outputs),
        }
    }
}

/// What a node's [`process`](Node::process) did to its outputs.
///
/// Firewheel's set, plus [`Masked`](Self::Masked) so a
/// multichannel node can flag individual channels. The executor acts on it:
/// masks are what downstream silence-skipping reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Every output was written. No claim about its content.
    Modified,
    /// Every output was written, and these channels are silent / constant.
    /// A claim the node must be able to back — the executor trusts it.
    Masked {
        /// Channels that are exact silence.
        silent: SilenceMask,
        /// Channels that hold one value throughout.
        constant: ConstantMask,
    },
    /// The node wrote nothing; this block's output is silence. The executor
    /// zeroes. Says nothing about the *next* block: a synth in a delayed
    /// attack, or a sample with leading silence under a held note, is
    /// `Silent` and still busy.
    Silent,
    /// As [`Silent`](Self::Silent), and the node has **no pending internal
    /// activity**: park it until an input arrives. The only status that lets
    /// the executor skip a node with event inputs (see `Executor`'s silence
    /// skip); a node that returns it must produce silence for as long as its
    /// inputs stay quiet.
    Idle,
    /// The node wrote only **sample 0** of each output; each is that value
    /// throughout. The executor fills the rest.
    Constant,
    /// The node wrote nothing; output `c` is input `c` (zero past the input
    /// width). The executor copies.
    Bypass,
}

/// What a node is told about *this* call besides its buffers.
#[derive(Clone, Copy, Debug)]
pub struct Cx<'a> {
    /// The block's environment.
    pub env: &'a Env,
    /// The compiled **arrival latency** at this node's inputs: how far behind
    /// the graph's own time its input signal is. A node reading the transport
    /// reads it this many frames earlier ([`transport_at`](Self::transport_at)),
    /// so a latent path and a direct path agree about which beat a sample
    /// belongs to.
    pub arrival: Latency,
    /// The transport's recent jumps, kept by the executor in a fixed set
    /// whatever the plan (see [`TransportHistory`] for how far back it is
    /// exact): what [`transport_at`](Self::transport_at) reads before the
    /// block's first frame. [`TransportHistory::none`] where nothing is
    /// kept.
    pub history: &'a TransportHistory,
    /// The project's meter for this block, lent once per block by the host
    /// that renders it, or `None` where the host has none (a hand-built
    /// `Cx`): a node counting bars then counts 4/4 from beat 0. One map per
    /// block, read at any beat the block visits, so a loop wrap and any
    /// number of meter changes inside the block are exact.
    pub meter: Option<&'a MeterMap>,
    /// The project's tempo map for this block, lent once per block by the
    /// host that renders it, or `None` where the host has none (a hand-built
    /// `Cx`): a node placing something by beat then takes the tempo of the
    /// block's transport ([`Env::transport`]) as it stands. One map per
    /// block, read at any beat the block visits, so how long a placed voice
    /// has played through a tempo step or ramp is the map's seconds.
    pub tempo: Option<&'a TempoMap>,
}

impl Cx<'_> {
    /// The transport at frame `offset` of this node's **input**: the
    /// transport [`arrival`](Self::arrival) frames before `offset`, which is
    /// when the audio arriving there was rendered
    /// ([`Env::transport_before`]). With no arrival it is
    /// [`Env::transport_at`]. A node that reports the playhead to something
    /// aligned with its input audio (a hosted plugin, a tempo-synced effect)
    /// reads this, not the block's own time.
    pub fn transport_at(&self, offset: Offset) -> TransportPosition {
        self.env
            .transport_before(offset, self.arrival, self.history)
    }

    /// The play clock at frame `offset` of this node's **input**: the clock
    /// [`arrival`](Self::arrival) frames before `offset`
    /// ([`Env::play_before`]), as [`transport_at`](Self::transport_at)
    /// answers for the transport. With no arrival it is [`Env::play_at`].
    /// Every lane and signal source reads the clock this way, so one free
    /// LFO routed to several targets stays phase-coherent across them.
    pub fn play_at(&self, offset: Offset) -> PlayReading {
        self.env.play_before(offset, self.arrival, self.history)
    }
}

/// What a node's report narrows its `ceiling` tail to: the shorter of the
/// two, or `None` when the report says nothing.
///
/// [`Tail::Unknown`] reported never narrows, and nothing narrows an `Unknown`
/// ceiling (it bounds nothing, so a report has nothing to clamp to). A report
/// above the ceiling is clamped to it; an `Unbounded` report under an
/// `Unbounded` ceiling stays `Unbounded` (never skipped).
pub(crate) fn narrow_tail(ceiling: Tail, reported: Tail) -> Option<Tail> {
    match (ceiling, reported) {
        (_, Tail::Unknown) | (Tail::Unknown, _) => None,
        (Tail::Unbounded, r) => Some(r),
        (c, Tail::Unbounded) => Some(c),
        (c, r) => Some(match (c.samples(), r.samples()) {
            (Some(c), Some(r)) if r < c => Tail::None.beside(Tail::Finite(r)),
            _ => c,
        }),
    }
}

/// A processor in the graph.
///
/// `F` is the sample format of the audio the node processes ([`Io<F>`](Io)):
/// `impl Node for X` is the `f32` node, and a node generic over the format is
/// `impl<F: Float> Node<F> for X`. The executor of a format runs the nodes of that
/// format. Object-safe: the executor stores `Box<dyn Node<F>>`. `Send + 'static` so a
/// unit can be built on the control thread and moved to the audio thread in a
/// commit; not `Sync`, because only the thread that runs a node ever
/// touches it.
///
/// # Implementing a node
///
/// [`shape`](Self::shape) says what the node looks like (ports, latency,
/// tail, event and param declarations); [`prepare`](Self::prepare) sizes its
/// buffers on the control thread; [`process`](Self::process) renders one
/// block on the audio thread and reports what it did to its outputs as a
/// [`Status`]; [`reset`](Self::reset) clears its state. Insert it through
/// [`IntoNode`] — for a node without controls, wrap it in
/// [`ForkByClone`](crate::ForkByClone) or [`Unforkable`](crate::Unforkable).
///
/// # A tail that follows the params
///
/// [`Shape::tail`] is the **ceiling**: the longest the node can ring, fixed at
/// [`prepare`](Self::prepare). A node whose ring-out is shorter under its
/// current settings (a reverb at a short decay) narrows it each block with
/// [`Io::report_tail`]. The report is the node's duty to get right: it must
/// cover the ring-out from the last non-quiet input frame under the settings
/// in force since. A node that shortens its decay keeps the longer figure
/// until its state has decayed, and one with a modulated decay reports from
/// the larger end of the block's ramp. The rule errs long, because a figure
/// too short lets the silence skip park a node that still sounds.
///
/// The figure is measured from the last non-quiet input frame, not from
/// the moment of the call. Only a node without event inputs is parked on its
/// tail (one with event inputs parks on its own [`Status::Idle`]), so only
/// such a node's report matters.
///
/// A report holds until the node's next call; a call that reports nothing
/// restores the ceiling, as does every new unit (insert, replace, fork,
/// re-prepare). The executor never calls [`reset`](Self::reset), so a node
/// whose `reset` changes its ring-out reports the new figure on its next call.
///
/// ```
/// use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
/// use tutti_buffers::ChannelLayout;
///
/// /// Multiplies its input by a fixed gain.
/// #[derive(Clone)]
/// struct Gain(f32);
///
/// impl Node for Gain {
///     fn shape(&self) -> Shape {
///         Shape::through_unassigned(ChannelLayout::MONO)
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         let (ins, mut outs) = io.split();
///         for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
///             *o = i * self.0;
///         }
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
/// ```
pub trait Node<F: Float = f32>: Send + 'static {
    /// Returns the node's ports, latency and tail, and its event and param
    /// declarations. Must not change after [`prepare`](Self::prepare)
    /// without the node being re-inserted — a shape change is a recompile.
    fn shape(&self) -> Shape;

    /// Sizes the node's buffers for `p`. Control thread; may allocate. Called before the
    /// first [`process`](Self::process), and again whenever the rate or the
    /// maximum block changes. The node is in no executor while prepared: the
    /// editor prepares a unit it holds (before placing it, or after the
    /// executor returned it), so no [`process`](Self::process) runs meanwhile.
    fn prepare(&mut self, p: &Prepare);

    /// Renders one block and reports what it did to the outputs. Audio
    /// thread: must not allocate, lock or block.
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_, F>) -> Status;

    /// Returns the node to the state of a freshly prepared one.
    fn reset(&mut self);

    /// Answers the handoffs a **parked** node has pending, once per block the
    /// executor parks it ([`NodeMode::Bypassed`](crate::NodeMode::Bypassed) with
    /// its fade done): [`process`](Self::process) is not called then, so a
    /// node that holds something the control side recalls (a plugin's
    /// Processor) would otherwise never hand it back.
    ///
    /// The contract is narrow: answer pending handoffs only. It runs at the node's place in the
    /// schedule, on the thread that would have run `process`, so every
    /// participant count sees the same. It is never called for a node
    /// that is not parked, nor for one the executor skips as idle (that one is
    /// woken through its [`Wake`] and processed). Audio thread: must not allocate, lock or block.
    ///
    /// The default does nothing.
    fn poll_parked(&mut self) {}

    /// Writes the state a successor must start from, as events for its first
    /// event input, when a replace swaps this node out (a crossfade, a plain
    /// replace, an insert at an existing key): an instrument's programs,
    /// controllers and bends as the MIDI messages that set them.
    ///
    /// The default writes nothing. A node that overrides it declares
    /// [`Shape::chase`], and the executor calls it **once, where the
    /// commit is applied**, before the block's first task runs: the events
    /// reach the successor at offset 0 of its first block, ahead of every
    /// other event there (they travel as flushed events, see the module
    /// docs of the executor), so a parallel render sees what a one-participant one
    /// does. The successor is chased whatever its type; one with no event
    /// input drops them, and the drop is counted
    /// ([`Executor::dropped_event_count`](crate::Executor::dropped_event_count)).
    /// During a crossfade only the incoming node is chased. A fork is not:
    /// a forked node carries its state in the copy. A replace that waits
    /// behind a crossfade already running is chased too, by the node fading
    /// in: it writes when the replace is applied and the executor holds what
    /// it wrote until that fade ends, then delivers it at offset 0 of the
    /// successor's first block, as for any other replace. Audio thread: must
    /// not allocate, lock or block.
    fn chase(&self, out: &mut Chase<'_>) {
        let _ = out;
    }

    /// The **base** of declared param `port` ([`Shape::params`]): the current
    /// value of the node's own control for it — the `Param<U>` its
    /// `Controls` hand out. The executor reads it once per block for a
    /// modulated param and ramps it across the block under the modulation;
    /// an unmodulated param is not asked (the node reads its control
    /// itself, [`ParamInput::Base`](crate::ParamInput::Base)).
    ///
    /// `None`, the default, is a node that cannot say: its param is then
    /// never modulated — it keeps reading [`ParamInput::Base`](crate::ParamInput::Base)
    /// whatever the graph connects — rather than riding on a base of 0,
    /// which would silence it. Audio thread:
    /// must not allocate, lock or block.
    fn param_base(&self, port: usize) -> Option<f64> {
        let _ = port;
        None
    }

    /// Receives the node's [`Wake`]: the handle that makes the executor call
    /// a node it parked.
    ///
    /// Control thread. The editor calls it **before** [`prepare`](Self::prepare)
    /// each time it places the node at its key or prepares it again (a
    /// re-prepare hands the same handle back), so `prepare` may already use
    /// it. Keep it, and give a clone to any thread that will need to wake the
    /// node (a plugin's `request_process` callback, a worker, a device
    /// thread); each calls [`Wake::notify`].
    ///
    /// **A notify wakes the node on the next block**: even with silent
    /// inputs, no events, and a last call that returned [`Status::Idle`], the
    /// node is called once on the first block the executor runs after the
    /// notify (and is parked again by the usual rules if it returns `Idle`
    /// again). Several notifies before that block wake it once. A notify on a
    /// node that is running, not parked, or mid-block is harmless: the next
    /// block consumes it. A node that never parks itself ignores this, which
    /// is the default. A node that wraps another forwards the call, or the
    /// inner node never hears of its handle.
    ///
    /// The handle is the placement's: a node it was replaced by, or a fork's
    /// copy, gets its own, and a notify on this one wakes neither.
    fn bind_wake(&mut self, wake: Wake) {
        let _ = wake;
    }
}

/// How a value becomes a node, and what the caller gets back to control it.
///
/// The *type* of the control
/// surface is fixed at insertion, so nothing downcasts to find a node's
/// parameters later. A node with live parameters implements this on a
/// builder type and returns its `Param<U>` handles.
///
/// # Every node says whether it forks
///
/// A node that can be copied into a graph of its own — an offline export, a
/// live duplicate ([`Editor::fork`](crate::Editor::fork)) — says so here, in
/// [`into_parts`](Self::into_parts), by handing the editor a
/// [`ForkSource`] beside the node. It is the only chance:
/// once inserted, the unit belongs to the executor and nothing on the control
/// side can reach it again.
///
/// So `into_parts` is **required**, and there is no blanket impl for a bare
/// [`Node`]: every insert states its fork-ability, and a node inserted
/// without saying cannot be written. This also leaves the impl slot free for
/// a node type that hands over its own fork source. A bare node is inserted as one of:
///
/// - [`ForkByClone`](crate::ForkByClone)`(node)` — forkable, by a clone
///   taken at insert (the node's `Clone` shares nothing with the original);
/// - [`Unforkable`](crate::Unforkable)`(node)` — refuses every fork that
///   needs it, by name ([`ForkError::NotForkable`](crate::ForkError::NotForkable));
/// - its own `IntoNode`, for a node with controls or a fork of its own.
///
/// ```compile_fail,E0277
/// # use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status};
/// # use tutti_units::{SampleRate, Samples};
/// # use tutti_buffers::ChannelLayout;
/// # use tutti_graph::NodeKey;
/// struct Silence;
/// impl Node for Silence {
///     fn shape(&self) -> Shape { Shape::source_unassigned(ChannelLayout::MONO) }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status { Status::Modified }
///     fn reset(&mut self) {}
/// }
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
/// // Forkable or not? Unsaid, so refused: `Silence: IntoNode` is not satisfied.
/// editor.insert(NodeKey(1), "silence", Silence);
/// ```
pub trait IntoNode<F: Float = f32> {
    /// What the caller keeps: `Param<U>` handles, `Publisher` cells, or `()`.
    type Controls;

    /// Splits the value into the unit the executor will own, the handles the caller
    /// keeps, and where a fork of the unit comes from
    /// ([`NodeParts::fork`]: `None` for a node that cannot be forked). What
    /// [`Editor::insert`](crate::Editor::insert) calls.
    ///
    /// Required, so an implementor always writes the `fork` field: a type
    /// that wraps another `IntoNode` forwards the inner one's parts rather
    /// than dropping its source by default.
    fn into_parts(self) -> NodeParts<Self::Controls, F>;

    /// [`into_parts`](Self::into_parts) without the fork source: the unit
    /// and its controls, for a caller that runs the unit outside an editor
    /// (a bench, a fork source building its fork's unit).
    fn into_node(self) -> (Box<dyn Node<F>>, Self::Controls)
    where
        Self: Sized,
    {
        let NodeParts { node, controls, .. } = self.into_parts();
        (node, controls)
    }

    /// The kind [`GraphBuilder::add`](crate::GraphBuilder::add) records for
    /// this node (what diagnostics print): its type name. A wrapper that
    /// only says how the node forks ([`ForkByClone`](crate::ForkByClone),
    /// [`Unforkable`](crate::Unforkable)) names the node it wraps, so the
    /// answer to "does it fork" does not rename it.
    fn kind() -> &'static str
    where
        Self: Sized,
    {
        std::any::type_name::<Self>()
    }
}

/// A node split for insertion: the unit, its controls, and where a fork of
/// it comes from. What [`IntoNode::into_parts`] returns, and itself an
/// [`IntoNode`], so a caller that had to split a node early (to read its
/// shape, as [`GraphBuilder`](crate::GraphBuilder) does) can still insert it
/// with its fork source.
///
/// Built with [`NodeParts::new`]. The fields are public because callers
/// destructure them.
pub struct NodeParts<C, F: Float = f32> {
    /// The unit the executor will own.
    pub node: Box<dyn Node<F>>,
    /// The handles the caller keeps.
    pub controls: C,
    /// Where a fork of this node comes from; `None` if it cannot be forked.
    pub fork: Option<Box<dyn ForkSource<F>>>,
    /// The node's params by address; `None` for a node with none.
    pub params: Option<ParamSet>,
    /// What frees the values the node's published cells retired; `None` for a
    /// node that publishes nothing.
    pub reclaimer: Option<Box<dyn Reclaim + Send>>,
    /// The node's published values, one per cell, each under its kind; empty
    /// for a node that publishes nothing.
    pub published: Vec<ErasedPublished>,
}

impl<C, F: Float> NodeParts<C, F> {
    /// The unit, its controls, its fork source and what frees what its
    /// published cells retired (`None` for a node that publishes nothing),
    /// each stated by the caller. No params until [`params`](Self::params)
    /// says so.
    ///
    /// The reclaimer is a clone of the node's controls (or of the cells they
    /// hold); the session runs it on every update, on the control thread, for
    /// as long as the node is in. A call that leaves it out does not compile:
    ///
    /// ```compile_fail,E0061
    /// # use tutti_graph::{Node, NodeParts};
    /// fn parts(node: Box<dyn Node>) -> NodeParts<()> {
    ///     NodeParts::new(node, (), None)
    /// }
    /// ```
    #[must_use]
    pub fn new(
        node: Box<dyn Node<F>>,
        controls: C,
        fork: Option<Box<dyn ForkSource<F>>>,
        reclaimer: Option<Box<dyn Reclaim + Send>>,
    ) -> Self {
        Self {
            node,
            controls,
            fork,
            params: None,
            reclaimer,
            published: Vec::new(),
        }
    }

    /// The node's params by address: what a session's param write goes
    /// through and what a fork of the node starts from.
    #[must_use]
    pub fn params(mut self, params: ParamSet) -> Self {
        self.params = Some(params);
        self
    }

    /// The node's published value of the kind `K`, read and replaced through
    /// `cell`, whose value is `K::Value`. `N` is the node that declares the
    /// kind ([`HasPublished`]). The cell's publisher belongs in the node's
    /// reclaimer too.
    #[must_use]
    pub fn published<N: HasPublished<K>, K: PublishedKind>(
        mut self,
        cell: Arc<dyn Published<Value = K::Value>>,
    ) -> Self {
        self.published.push(ErasedPublished::new::<K>(cell));
        self
    }
}

impl<C, F: Float> std::fmt::Debug for NodeParts<C, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeParts")
            .field("forkable", &self.fork.is_some())
            .finish_non_exhaustive()
    }
}

impl<C, F: Float> IntoNode<F> for NodeParts<C, F> {
    type Controls = C;

    fn into_parts(self) -> NodeParts<C, F> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tutti_units::Samples;

    /// `all` and `covers` agree at the edges, including the 64-channel cap.
    #[test]
    fn masks_cover_their_width() {
        assert_eq!(SilenceMask::all(0), SilenceMask(0));
        assert_eq!(SilenceMask::all(3), SilenceMask(0b111));
        assert_eq!(SilenceMask::all(64), SilenceMask(u64::MAX));
        assert!(SilenceMask(0b111).covers(3));
        assert!(!SilenceMask(0b101).covers(3));
        assert!(SilenceMask::NONE.covers(0));
        assert!(!SilenceMask::NONE.with(70).get(70));
        assert!(SilenceMask::NONE.with(5).get(5));
    }

    /// A prepare carries the host's device quantum only when told, and a
    /// different quantum is a different prepare (so a re-prepare to it is not
    /// skipped as a no-op).
    #[test]
    fn a_prepare_carries_the_device_quantum_it_is_given() {
        let p = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(1024)));
        assert_eq!(p.quantum(), None);
        let q = p.with_quantum(MaxBlock::of(Samples::new(480)));
        assert_eq!(q.quantum(), Some(MaxBlock::of(Samples::new(480))));
        assert_eq!(q.max_block(), p.max_block());
        assert_ne!(p, q);
    }

    /// Scratch built from a `MaxBlock` serves every block up to it, and
    /// refuses past it rather than truncating.
    #[test]
    fn scratch_fits_every_prepared_block() {
        let p = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(100)));
        let mut s = Scratch::<f32>::new(p.max_block(), 2);
        assert_eq!(s.channel(1, 100).len(), 100);
        assert_eq!(s.channel(0, 7).len(), 7);
        let over = std::panic::catch_unwind(move || {
            let _ = s.channel(0, 101);
        });
        assert!(over.is_err(), "a block past MaxBlock is refused");
    }
}
