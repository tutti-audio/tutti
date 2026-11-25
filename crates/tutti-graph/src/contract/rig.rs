//! [`BlockRig`] and [`Direct`]: one node driven block by block without
//! allocating.

use crate::NodeKey;
use crate::OutputPort;
use crate::{InEvents, OutEvents};
use tutti_buffers::MaxBlock;
use tutti_buffers::{BufferError, Float, Planes, PlanesMut};
use tutti_buffers::{ChannelLayout, ChannelSet};
use tutti_rt::CappedVec;
use tutti_units::{Frame, Latency, SampleRate, Samples};

use crate::contract::drive::{HAND_EVENT_CAPACITY, param_room};
use crate::edges::rtlog::{LogCapture, LogFilter, RtRecord};
use crate::edit::editor::Editor;
use crate::event::sorted::SortedEvents;
use crate::event::types::Event;
use crate::event::writer::EventWriter;
use crate::exec::Executor;
use crate::exec::history::TransportHistory;
use crate::node::io::Io;
use crate::node::param::ParamInput;
use crate::node::param_event::ParamEvent;
use crate::node::time::{Env, TransportChanges, TransportPosition};
use crate::node::{Cx, IntoNode, Node, Prepare, Status};

/// A rig's view of its channels: the empty view for no channel (a node with
/// no audio on that side), and a panic for any other refusal, which a rig's own
/// buffers never produce.
pub(super) fn or_empty<T: Default>(planes: Result<T, BufferError>) -> T {
    match planes {
        Ok(planes) => planes,
        Err(BufferError::ZeroWidth) => T::default(),
        Err(e) => panic!("a rig's planes are well formed: {e}"),
    }
}

/// The widest node [`BlockRig`] drives: twice a bus ([`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS)),
/// so a test can drive a node wider than a bus. Its per-block slice lists live
/// on the stack, so the rig allocates nothing per block.
pub const RIG_MAX_CHANNELS: usize = 2 * tutti_buffers::MAX_CHANNELS;

/// One node alone in a graph, driven block by block **without allocating**:
/// the harness for a node's allocation gate (a render prepared
/// [`Realtime`](crate::RenderMode::Realtime) runs inside
/// [`RtScope`](tutti_rt::RtScope)). Its inputs read [`inputs_mut`](Self::inputs_mut)
/// and its outputs land in [`output`](Self::output), channel for channel.
///
/// Everything is built in [`new`](Self::new); a block after that touches
/// only the executor and buffers sized there.
pub struct BlockRig<F: Float = f32> {
    _editor: Editor<F>,
    exec: Executor<F>,
    frames: usize,
    inputs: Vec<Vec<F>>,
    outputs: Vec<Vec<F>>,
}

impl<F: Float> std::fmt::Debug for BlockRig<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockRig")
            .field("_editor", &self._editor)
            .field("exec", &self.exec)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl BlockRig {
    /// `node`, alone, in an `f32` rig prepared at `rate` for blocks of
    /// `frames`, with silent inputs. Returns its controls.
    /// [`with_format`](BlockRig::with_format) builds the rig for another
    /// sample format.
    ///
    /// # Panics
    ///
    /// As [`with_format`](BlockRig::with_format).
    pub fn new<N: IntoNode>(node: N, rate: SampleRate, frames: usize) -> (Self, N::Controls) {
        Self::with_format(node, rate, frames)
    }
}

impl<F: Float> BlockRig<F> {
    /// [`new`](BlockRig::new) in sample format `F`.
    ///
    /// # Panics
    ///
    /// If the node is wider than [`RIG_MAX_CHANNELS`] either way, or the
    /// one-node graph does not commit.
    pub fn with_format<N: IntoNode<F>>(
        node: N,
        rate: SampleRate,
        frames: usize,
    ) -> (Self, N::Controls) {
        let (mut editor, mut exec) =
            Editor::<F>::with_format(Prepare::new(rate, MaxBlock::of(Samples::new(frames))));
        let key = NodeKey(1);
        let controls = editor.insert(key, N::kind(), node).controls;
        let shape = editor
            .spec()
            .topology
            .nodes
            .get(&key)
            .map(|n| (n.inputs, n.outputs))
            .expect("inserted");
        let (ins, outs) = (usize::from(shape.0.count()), usize::from(shape.1.count()));
        assert!(
            ins <= RIG_MAX_CHANNELS && outs <= RIG_MAX_CHANNELS,
            "a rig drives at most {RIG_MAX_CHANNELS} channels a side"
        );
        editor
            .declare_main_output(
                ChannelSet::default_for(ChannelLayout::clamped(outs)),
                editor.spec().main_output_id.next(),
            )
            .expect("a new id declares anything");
        let topology = &mut editor.spec_mut().topology;
        topology.inputs =
            ChannelLayout::try_from(ins).expect("asserted within RIG_MAX_CHANNELS above");
        for c in 0..ins {
            topology.connect(
                crate::InputPort::new(key, c as u16),
                crate::Edge::Direct(crate::PortSource::MainInput(c as u16)),
            );
        }
        topology.outputs = (0..outs)
            .map(|c| vec![crate::PortSource::Node(OutputPort::new(key, c as u16))])
            .collect();
        editor.commit().expect("a one-node graph commits");
        exec.apply_pending();
        editor.collect();
        (
            Self {
                _editor: editor,
                exec,
                frames,
                inputs: vec![vec![F::ZERO; frames]; ins],
                outputs: vec![vec![F::ZERO; frames]; outs],
            },
            controls,
        )
    }

    /// The input channels, to fill before a block (empty for a node with no
    /// audio input).
    pub fn inputs_mut(&mut self) -> PlanesMut<'_, '_, F> {
        or_empty(PlanesMut::from_vecs(&mut self.inputs))
    }

    /// The output channels of the last block (empty for a node with no
    /// audio output).
    pub fn output(&self) -> Planes<'_, F> {
        or_empty(Planes::from_vecs(&self.outputs))
    }

    /// One block, transport stopped. Allocation-free.
    pub fn block(&mut self) {
        self.block_in(&TransportPosition::default());
    }

    /// One block under `transport` (a rolling one, for a node that reads
    /// the transport from its `Env`). Allocation-free.
    pub fn block_in(&mut self, transport: &TransportPosition) {
        let mut ins: [&[F]; RIG_MAX_CHANNELS] = [&[]; RIG_MAX_CHANNELS];
        for (slot, c) in ins.iter_mut().zip(&self.inputs) {
            *slot = c;
        }
        let (n_in, n_out) = (self.inputs.len(), self.outputs.len());
        let mut outs = self.outputs.iter_mut();
        let mut out_refs: [&mut [F]; RIG_MAX_CHANNELS] =
            core::array::from_fn(|_| outs.next().map_or(&mut [][..], |c| &mut c[..]));
        self.exec
            .process(self.frames, transport, &ins[..n_in], &mut out_refs[..n_out]);
    }
}

/// One node called by hand, block after block, **without allocating** —
/// [`drive_input`](crate::contract::drive_input) with its buffers built once. For a node's bench, or an
/// allocation gate that needs its param ports fed or its events played
/// (which [`BlockRig`], with no param or event source, cannot). Inputs,
/// params and outputs are buffers of the prepared frames; fill them between
/// blocks. Each block starts where the last ended (its [`Env::frame`]), under
/// the transport last [set](Self::set_transport) (stopped, by default).
pub struct Direct<N: ?Sized, F: Float = f32> {
    /// The node, prepared.
    pub node: Box<N>,
    prepare: Prepare,
    len: usize,
    frame: Frame,
    transport: TransportPosition,
    inputs: Vec<Vec<F>>,
    params: Vec<Option<Vec<F>>>,
    /// `params` widened.
    wide: Vec<Vec<f64>>,
    outputs: Vec<Vec<F>>,
    events_in: Vec<Vec<Event>>,
    events_out: Vec<CappedVec<Event>>,
    params_in: Vec<Vec<ParamEvent>>,
    params_out: Vec<CappedVec<ParamEvent>>,
    /// What the node reported with `Io::count_lost`, over every block.
    lost: u64,
    /// The records the node logged, when [`log`](Direct::log) is set.
    capture: Option<LogCapture>,
    logged: Vec<RtRecord>,
}

impl<N: ?Sized, F: Float> std::fmt::Debug for Direct<N, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Direct")
            .field("prepare", &self.prepare)
            .field("len", &self.len)
            .field("frame", &self.frame)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

impl<N: Node + ?Sized> Direct<N> {
    /// `node`, in an `f32` driver prepared at `rate` for blocks of `frames`,
    /// silent inputs, no param fed, no events.
    /// [`with_format`](Direct::with_format) builds the driver for another
    /// sample format.
    ///
    /// # Panics
    ///
    /// If the node is wider than [`RIG_MAX_CHANNELS`] either way, or has more
    /// than [`MAX_PORTS`](crate::MAX_PORTS) event ports a side.
    pub fn new(node: N, rate: SampleRate, frames: usize) -> Self
    where
        N: Sized,
    {
        Self::with_format(node, rate, frames)
    }

    /// [`new`](Self::new) for a node an `IntoNode` already boxed, e.g.
    /// `Direct::boxed(x.into_node().0, ..)`.
    ///
    /// # Panics
    ///
    /// As [`new`](Self::new).
    pub fn boxed(node: Box<N>, rate: SampleRate, frames: usize) -> Self {
        Self::boxed_with_format(node, rate, frames)
    }

    /// [`new`](Self::new) prepared under `prepare`: its rate, its largest block
    /// and its [`RenderMode`](crate::RenderMode). A
    /// [`Realtime`](crate::RenderMode::Realtime) driver runs `process` inside
    /// [`RtScope`](tutti_rt::RtScope), an [`Offline`](crate::RenderMode::Offline)
    /// one outside it; [`new`](Self::new) prepares realtime.
    ///
    /// # Panics
    ///
    /// As [`new`](Self::new).
    pub fn with_prepare(node: N, prepare: &Prepare) -> Self
    where
        N: Sized,
    {
        Self::boxed_with_prepare(Box::new(node), prepare)
    }
}

impl<N: Node<F> + ?Sized, F: Float> Direct<N, F> {
    /// [`new`](Direct::new) in sample format `F`.
    ///
    /// # Panics
    ///
    /// As [`new`](Direct::new).
    pub fn with_format(node: N, rate: SampleRate, frames: usize) -> Self
    where
        N: Sized,
    {
        Self::boxed_with_format(Box::new(node), rate, frames)
    }

    /// [`boxed`](Direct::boxed) in sample format `F`.
    ///
    /// # Panics
    ///
    /// As [`new`](Direct::new).
    pub fn boxed_with_format(node: Box<N>, rate: SampleRate, frames: usize) -> Self {
        Self::boxed_with_prepare(
            node,
            &Prepare::new(rate, MaxBlock::of(Samples::new(frames))),
        )
    }

    /// [`boxed_with_format`](Self::boxed_with_format) prepared under `prepare`
    /// (its rate, its largest block and its [`RenderMode`](crate::RenderMode)):
    /// a [`Realtime`](crate::RenderMode::Realtime) driver runs `process` inside
    /// [`RtScope`](tutti_rt::RtScope), an [`Offline`](crate::RenderMode::Offline)
    /// one outside it.
    ///
    /// # Panics
    ///
    /// As [`new`](Direct::new).
    pub fn boxed_with_prepare(mut node: Box<N>, prepare: &Prepare) -> Self {
        let frames = prepare.max_block().get();
        node.prepare(prepare);
        let shape = node.shape();
        let (ins, outs) = (
            usize::from(shape.audio_in().count()),
            usize::from(shape.audio_out().count()),
        );
        assert!(
            ins <= RIG_MAX_CHANNELS && outs <= RIG_MAX_CHANNELS,
            "a direct driver drives at most {RIG_MAX_CHANNELS} channels a side"
        );
        assert!(
            usize::from(shape.event_in) <= crate::node::MAX_PORTS
                && usize::from(shape.event_out) <= crate::node::MAX_PORTS,
            "a direct driver drives at most {} event ports a side",
            crate::node::MAX_PORTS
        );
        let events = |n: u16| {
            (0..usize::from(n))
                .map(|_| Vec::with_capacity(HAND_EVENT_CAPACITY))
                .collect()
        };
        let param_events = |n: u16, room: usize| {
            (0..usize::from(n))
                .map(|_| Vec::with_capacity(room))
                .collect()
        };
        Self {
            params: vec![None; shape.params.as_slice().len()],
            wide: vec![Vec::new(); shape.params.as_slice().len()],
            events_in: events(shape.event_in),
            events_out: (0..usize::from(shape.event_out))
                .map(|_| CappedVec::with_capacity(HAND_EVENT_CAPACITY))
                .collect(),
            params_in: param_events(shape.event_in, HAND_EVENT_CAPACITY),
            params_out: (0..usize::from(shape.event_out))
                .map(|_| CappedVec::with_capacity(param_room(&shape)))
                .collect(),
            node,
            prepare: *prepare,
            len: frames,
            frame: Frame::new(0),
            transport: TransportPosition::default(),
            inputs: vec![vec![F::ZERO; frames]; ins],
            outputs: vec![vec![F::ZERO; frames]; outs],
            lost: 0,
            capture: None,
            logged: Vec::new(),
        }
    }

    /// Keeps what the node writes through `Io::log`, `Io::log_plugin` and
    /// `Io::log_line` that `filter` lets in (a driver built without this
    /// drops every record). After each block the records move, in order and
    /// coalesced as the executor's slots coalesce them, onto
    /// [`logged`](Self::logged); a record past a node call's fourth is
    /// counted in [`log_lost`](Self::log_lost). Each record's
    /// [`source`](RtRecord::source) is `NodeKey(0)`, the one placeholder
    /// key, and its [`frame`](RtRecord::frame) is the block's first frame
    /// plus the offset the call gave, as the executor's flush fills them.
    #[must_use]
    pub fn log(mut self, filter: LogFilter) -> Self {
        self.capture = Some(LogCapture::new(filter));
        self
    }

    /// The records the node logged over every block since
    /// [`log`](Self::log) was set, oldest first. Cumulative: reading it does
    /// not drain it.
    pub fn logged(&self) -> &[RtRecord] {
        &self.logged
    }

    /// The records the node logged past a call's fourth distinct one, over
    /// every block since [`log`](Self::log) was set; zero without it.
    pub fn log_lost(&self) -> u64 {
        self.capture.as_ref().map_or(0, LogCapture::lost)
    }

    /// The events the node reported lost (`Io::count_lost`) over every block
    /// since this driver was built: what an executor adds to its lost-event
    /// count. Cumulative: reading it does not drain or reset it.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// The input channels, to fill before a block (empty for a node with no
    /// audio input).
    pub fn inputs_mut(&mut self) -> PlanesMut<'_, '_, F> {
        or_empty(PlanesMut::from_vecs(&mut self.inputs))
    }

    /// Feeds declared param `k` with `values` (one per frame) from the next
    /// block on, or `None` to read its base again.
    ///
    /// # Panics
    ///
    /// If `values` is not one block long.
    pub fn feed(&mut self, k: usize, values: Option<&[F]>) {
        if let Some(v) = values {
            assert_eq!(
                v.len(),
                self.prepare.max_block().get(),
                "one value per frame"
            );
        }
        self.params[k] = values.map(<[F]>::to_vec);
        self.wide[k] = values.map_or_else(Vec::new, |v| v.iter().map(|x| x.to_f64()).collect());
    }

    /// Play `events` on event input `port` in the **next** block only (each
    /// block starts with every port empty). Up to [`HAND_EVENT_CAPACITY`]
    /// per port without allocating. Sorted and inside the next block, or
    /// that block panics.
    pub fn events(&mut self, port: InEvents, events: &[Event]) {
        let list = &mut self.events_in[port.index()];
        list.clear();
        list.extend_from_slice(events);
    }

    /// Play the param events `events` on event input `port` in the **next**
    /// block only, beside its [`events`](Self::events). Up to
    /// [`HAND_EVENT_CAPACITY`] per port without allocating. Sorted and inside
    /// the next block, or that block panics.
    pub fn param_events(&mut self, port: InEvents, events: &[ParamEvent]) {
        let list = &mut self.params_in[port.index()];
        list.clear();
        list.extend_from_slice(events);
    }

    /// What the node wrote to event output `port` in the last block.
    pub fn event_output(&self, port: OutEvents) -> &[Event] {
        &self.events_out[port.index()]
    }

    /// The param events the node wrote to event output `port` in the last
    /// block.
    pub fn param_events_out(&self, port: OutEvents) -> &[ParamEvent] {
        &self.params_out[port.index()]
    }

    /// The transport of every block from the next one on: its position is
    /// the next block's first frame's (a rolling transport is **not**
    /// advanced from block to block; set it again to move it).
    pub fn set_transport(&mut self, transport: TransportPosition) {
        self.transport = transport;
    }

    /// Runs the blocks from the next one on `len` frames long, at most the
    /// prepared frames (default: all of them). Inputs, param feeds and
    /// outputs are read and written in their first `len` frames.
    ///
    /// # Panics
    ///
    /// If `len` is zero or past the prepared frames.
    pub fn set_block_len(&mut self, len: usize) {
        assert!(
            len > 0 && len <= self.prepare.max_block().get(),
            "a block of {len} frames against a prepared maximum of {}",
            self.prepare.max_block().get()
        );
        self.len = len;
    }

    /// The output channels of the last block (the block's length; empty for
    /// a node with no audio output).
    pub fn output(&self) -> Planes<'_, F> {
        if self.outputs.is_empty() {
            return Planes::default();
        }
        or_empty(Planes::from_vecs(&self.outputs)).slice(Samples::new(0)..Samples::new(self.len))
    }

    /// The output channels, to fill before a block: a node must write every
    /// frame of every output it declares, and what it leaves shows (empty for
    /// a node with no audio output).
    pub fn outputs_mut(&mut self) -> PlanesMut<'_, '_, F> {
        or_empty(PlanesMut::from_vecs(&mut self.outputs))
    }

    /// One block: at the driver's frame (advanced block to block), under
    /// its transport ([`set_transport`](Self::set_transport)), its block
    /// length ([`set_block_len`](Self::set_block_len)). Allocation-free.
    pub fn block(&mut self) -> Status {
        let env = Env {
            frame: self.frame,
            sample_rate: self.prepare.sample_rate(),
            block_len: Samples::new(self.len),
            transport: self.transport,
            changes: TransportChanges::NONE,
            // The driver's frame, as an executor's play clock would read
            // with no play start since its first block.
            play: crate::PlayClock {
                start: crate::PlayReading {
                    epoch: 0,
                    play: self.frame,
                },
                next_epoch: 0,
            },
        };
        let status = self.block_in(&env);
        self.frame = Frame::new(self.frame.get() + self.len as u64);
        status
    }

    /// One block under `env` exactly — its frame, rate, length, transport
    /// and the transport's changes inside the block — for a node that reads
    /// the transport from its `Env` (a clip reader crossing a start or a
    /// seek). The driver's own frame is not advanced; the block's length
    /// becomes the driver's ([`output`](Self::output) reads that many).
    /// Allocation-free.
    ///
    /// # Panics
    ///
    /// If `env.block_len` is zero or past the frames the driver was built
    /// for.
    pub fn block_in(&mut self, env: &Env) -> Status {
        self.set_block_len(env.block_len.get());
        let len = self.len;
        let mut ins: [&[F]; RIG_MAX_CHANNELS] = [&[]; RIG_MAX_CHANNELS];
        for (slot, c) in ins.iter_mut().zip(&self.inputs) {
            *slot = &c[..len];
        }
        let (n_in, n_out, n_par) = (self.inputs.len(), self.outputs.len(), self.params.len());
        let mut outs = self.outputs.iter_mut();
        let mut out_refs: [&mut [F]; RIG_MAX_CHANNELS] =
            core::array::from_fn(|_| outs.next().map_or(&mut [][..], |c| &mut c[..len]));
        let mut params = [ParamInput::Base; crate::node::param::MAX_PARAM_PORTS];
        for ((slot, p), wide) in params.iter_mut().zip(&self.params).zip(&self.wide) {
            if let Some(v) = p {
                *slot = ParamInput::Frames {
                    values: &wide[..len],
                    narrow: &v[..len],
                    corners: &[],
                };
            }
        }
        let mut sorted = [SortedEvents::EMPTY; crate::node::MAX_PORTS];
        for ((slot, list), params) in sorted.iter_mut().zip(&self.events_in).zip(&self.params_in) {
            *slot =
                SortedEvents::with_both(list, params, len).expect("sorted events inside the block");
        }
        let dropped = crate::event::writer::PortCount::default();
        let lost = core::cell::Cell::new(0u32);
        let n_ev_out = self.events_out.len();
        let mut bufs = self.events_out.iter_mut().zip(&mut self.params_out);
        let mut writers: [EventWriter<'_>; crate::node::MAX_PORTS] = core::array::from_fn(|_| {
            bufs.next().map_or(EventWriter::detached(), |(b, p)| {
                b.clear();
                p.clear();
                EventWriter::new(b, p, HAND_EVENT_CAPACITY, len as u32, &dropped)
            })
        });
        let cx = Cx {
            env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
            meter: None,
            tempo: None,
        };
        let max = self.prepare.max_block();
        let io = Io::new(
            max,
            len,
            &ins[..n_in],
            &mut out_refs[..n_out],
            crate::node::SilenceMask::NONE,
            crate::node::ConstantMask::NONE,
            crate::node::InPlaceMask::NONE,
            &sorted[..self.events_in.len()],
            &mut writers[..n_ev_out],
        )
        .with_main(self.node.shape().main_ports())
        .params(&params[..n_par])
        .with_lost(&lost);
        let io = match &self.capture {
            Some(capture) => io.with_log(capture.begin_block()),
            None => io,
        };
        let status = {
            let _rt = self.prepare.enter_rt_scope();
            self.node.process(&cx, io)
        };
        if let Some(capture) = &mut self.capture {
            capture.end_block(NodeKey(0), env.frame, &mut self.logged);
        }
        self.lost += u64::from(lost.get());
        for list in &mut self.events_in {
            list.clear();
        }
        for list in &mut self.params_in {
            list.clear();
        }
        status
    }
}
