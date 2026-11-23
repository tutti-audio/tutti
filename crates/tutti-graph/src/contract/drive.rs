//! A node called by hand, one block at a time, with no graph around it.

use tutti_buffers::MaxBlock;
use tutti_buffers::{Float, Planes};
use tutti_rt::CappedVec;
use tutti_units::{Frame, Latency, SampleRate, Samples, Tail, UnitAddr};

use crate::contract::rig::or_empty;
use crate::event::sorted::SortedEvents;
use crate::event::types::Event;
use crate::event::writer::EventWriter;
use crate::exec::history::TransportHistory;
use crate::node::controls::{ParamFork, ParamNode};
use crate::node::io::Io;
use crate::node::param::ParamInput;
use crate::node::param_event::ParamEvent;
use crate::node::shape::Shape;
use crate::node::time::{Env, TransportChanges, TransportPosition};
use crate::node::{Cx, Node, Prepare, Status};

/// One block of `node`, called by hand with no graph around it: `inputs`
/// on its audio inputs (one plane per port; their length is the block's),
/// `params[k]` on its declared param `k` (`None`, or a port past the end,
/// reads its base), no events, transport stopped at frame 0. One `Vec` per
/// audio output. A node with no audio input takes [`Planes::default`], and
/// then the block is as long as the first fed param.
///
/// For a node's own unit tests, which inspect its state between blocks —
/// what a [`Renderer::alone`](crate::Renderer::alone), whose node the executor owns, cannot show.
/// `node` must already be [`prepare`](Node::prepare)d for a maximum block
/// at least this long ([`prepared`]). [`drive_input`] is the same call under a
/// transport, with events.
///
/// # Panics
///
/// If the block is empty (a node is never called with zero frames), or the
/// input count is not the node's.
pub fn drive<F: Float>(
    node: &mut dyn Node<F>,
    rate: SampleRate,
    inputs: Planes<'_, F>,
    params: &[Option<&[F]>],
) -> Vec<Vec<F>> {
    let frames = (inputs.channels() > 0)
        .then(|| inputs.frames().get())
        .or_else(|| params.iter().flatten().next().map(|p| p.len()))
        .expect("a block needs a length: an input or a param");
    let env = Env {
        frame: Frame::new(0),
        sample_rate: rate,
        block_len: Samples::new(frames),
        transport: TransportPosition::default(),
        changes: TransportChanges::NONE,
        play: crate::PlayClock::ZERO,
    };
    drive_input(node, &env, inputs, params, &[]).audio
}

/// What [`drive_input`] rendered: each audio output, and what the node wrote
/// to each event output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Driven<F: Float = f32> {
    /// One `Vec` per audio output, the block long.
    pub audio: Vec<Vec<F>>,
    /// One `Vec` per event output port, in the order the node wrote them.
    pub events: Vec<Vec<Event>>,
    /// One `Vec` per event output port of the param events the node wrote, in
    /// the order it wrote them.
    pub params: Vec<Vec<ParamEvent>>,
    /// What the node passed to [`Io::report_tail`] in the call, if anything.
    pub reported_tail: Option<Tail>,
    /// What the call returned (`None` only in a `Driven::default()`), for a test
    /// of a node's silence claims: [`Status::Idle`] is the one that parks it.
    pub status: Option<Status>,
}

/// The most events [`drive_input`] and [`Direct`](crate::contract::Direct) accept on each event output
/// port per block: more than any node in the tree writes in one.
pub const HAND_EVENT_CAPACITY: usize = 1024;

/// The param events a hand-driven port holds: [`HAND_EVENT_CAPACITY`] for a
/// node that declares it writes them ([`Shape::param_events`]), none for
/// another.
pub(super) fn param_room(shape: &Shape) -> usize {
    if shape.param_events {
        HAND_EVENT_CAPACITY
    } else {
        0
    }
}

/// One block of `node` by hand, as a graph hands it one: under `env` (its
/// frame, length, transport and transport changes), `inputs` and `params`
/// as for [`drive`], and `events[p]` on event input `p` (a port past the
/// end is empty). The block is `env.block_len` long.
///
/// For a node that reads its block's transport ([`Env::transport_at`], its
/// changes) or plays events, in its own unit tests: what [`drive`], with the
/// transport stopped and no events, cannot reach.
///
/// # Panics
///
/// If the block is empty, an input is not the block long or the input
/// count is not the node's, or an event list is unsorted or reaches past
/// the block ([`SortedEvents::new`](crate::SortedEvents::new)).
pub fn drive_input<F: Float>(
    node: &mut dyn Node<F>,
    env: &Env,
    inputs: Planes<'_, F>,
    params: &[Option<&[F]>],
    events: &[&[Event]],
) -> Driven<F> {
    drive_input_params(node, env, inputs, params, events, &[])
}

/// [`drive_input`] for a node prepared under `prepare`: a
/// [`Realtime`](crate::RenderMode::Realtime) one runs its `process` inside
/// [`RtScope`](tutti_rt::RtScope), an [`Offline`](crate::RenderMode::Offline)
/// one outside it. [`drive_input`] drives as a realtime prepare does.
///
/// # Panics
///
/// As [`drive_input`].
pub fn drive_input_in<F: Float>(
    node: &mut dyn Node<F>,
    prepare: &Prepare,
    env: &Env,
    inputs: Planes<'_, F>,
    params: &[Option<&[F]>],
    events: &[&[Event]],
) -> Driven<F> {
    drive_block(node, prepare, env, inputs, params, events, &[])
}

/// [`drive_input`] with `param_events[p]` the param events on event input `p`
/// (a port past the end has none), beside its `events`.
///
/// # Panics
///
/// As [`drive_input`], and when a param event list is unsorted or reaches past
/// the block ([`SortedEvents::with_both`](crate::SortedEvents::with_both)).
pub fn drive_input_params<F: Float>(
    node: &mut dyn Node<F>,
    env: &Env,
    inputs: Planes<'_, F>,
    params: &[Option<&[F]>],
    events: &[&[Event]],
    param_events: &[&[ParamEvent]],
) -> Driven<F> {
    let prepare = Prepare::new(
        env.sample_rate,
        MaxBlock::of(Samples::new(env.block_len.get())),
    );
    drive_block(node, &prepare, env, inputs, params, events, param_events)
}

fn drive_block<F: Float>(
    node: &mut dyn Node<F>,
    prepare: &Prepare,
    env: &Env,
    inputs: Planes<'_, F>,
    params: &[Option<&[F]>],
    events: &[&[Event]],
    param_events: &[&[ParamEvent]],
) -> Driven<F> {
    let shape = node.shape();
    let frames = env.block_len.get();
    assert!(frames > 0, "a node is never called with zero frames");
    assert_eq!(
        inputs.channels(),
        usize::from(shape.audio_in().count()),
        "one plane per audio input"
    );
    assert!(
        (inputs.channels() == 0 || inputs.frames().get() == frames)
            && params.iter().flatten().all(|p| p.len() == frames),
        "every input plane and param slice is the block long"
    );
    let ins: Vec<&[F]> = inputs.iter().map(|c| c.samples()).collect();
    let mut out = vec![vec![F::ZERO; frames]; usize::from(shape.audio_out().count())];
    let mut refs: Vec<&mut [F]> = out.iter_mut().map(|c| &mut c[..]).collect();
    let params: Vec<ParamInput<'_, F>> = params
        .iter()
        .map(|p| {
            p.map_or(ParamInput::Base, |values| ParamInput::Frames {
                values,
                corners: &[],
            })
        })
        .collect();
    let events_in: Vec<SortedEvents<'_>> = (0..usize::from(shape.event_in))
        .map(|p| {
            let list = events.get(p).copied().unwrap_or(&[]);
            let plist = param_events.get(p).copied().unwrap_or(&[]);
            SortedEvents::with_both(list, plist, frames).expect("sorted events inside the block")
        })
        .collect();
    let mut out_events: Vec<CappedVec<Event>> = (0..usize::from(shape.event_out))
        .map(|_| CappedVec::with_capacity(HAND_EVENT_CAPACITY))
        .collect();
    let mut out_params: Vec<CappedVec<ParamEvent>> = (0..usize::from(shape.event_out))
        .map(|_| CappedVec::with_capacity(param_room(&shape)))
        .collect();
    let dropped = crate::event::writer::PortCount::default();
    let mut writers: Vec<EventWriter<'_>> = out_events
        .iter_mut()
        .zip(&mut out_params)
        .map(|(b, p)| EventWriter::new(b, p, HAND_EVENT_CAPACITY, frames as u32, &dropped))
        .collect();
    let cx = Cx {
        env,
        arrival: Latency::ZERO,
        history: TransportHistory::none(),
        meter: None,
        tempo: None,
    };
    let max = prepare.max_block();
    let reported = std::cell::Cell::new(None);
    let main = shape.main_ports();
    let io = Io::new(
        max,
        frames,
        &ins,
        &mut refs,
        crate::node::SilenceMask::NONE,
        crate::node::ConstantMask::NONE,
        crate::node::InPlaceMask::NONE,
        &events_in,
        &mut writers,
    )
    .with_main(main)
    .params(&params)
    .tail(&reported);
    let status = {
        let _rt = prepare.enter_rt_scope();
        node.process(&cx, io)
    };
    Driven {
        audio: out,
        events: out_events.iter().map(|e| e.to_vec()).collect(),
        params: out_params.iter().map(|p| p.to_vec()).collect(),
        reported_tail: reported.get(),
        status: Some(status),
    }
}

/// Checks the duty of a node that reports its tail
/// ([`Io::report_tail`]): after an impulse on every input channel at the
/// first frame of a block, its output is **exactly silent** past the tail it
/// reported, measured from that input frame (the last non-quiet one), plus
/// its declared latency, and past its ceiling ([`Shape::tail`]) whether or
/// not it reports. A node that reports too short is parked by the executor
/// while it still sounds, so this is the safety net the executor's skip does
/// not have.
///
/// The impulse is a sample of `1.0`; the node is driven by [`drive_input`] in
/// blocks of 64 frames with silent input after it, and must already be
/// [`prepare`](Node::prepare)d ([`prepared`]). Frame `0` is the impulse, so a
/// tail of `T` allows sound on frames `0..=T` and requires silence on every
/// frame after. An `Unbounded` or `Unknown` figure is not checkable and
/// passes; a figure above [`MAX_CHECKED_TAIL`] is refused as unchecked rather
/// than rendered.
///
/// # Panics
///
/// On the first sound after the tail, naming the frame and channel, or when
/// the figure to check is too long to render here.
pub fn assert_declared_tail(node: &mut dyn Node, rate: SampleRate) {
    const BLOCK: usize = 64;
    let shape = node.shape();
    let width = usize::from(shape.audio_in().count());
    let latency = shape.latency.samples().get();
    let env = |frame: u64| Env {
        frame: Frame::new(frame),
        sample_rate: rate,
        block_len: Samples::new(BLOCK),
        transport: TransportPosition::default(),
        changes: TransportChanges::NONE,
        play: crate::PlayClock::ZERO,
    };
    let mut impulse = vec![0.0f32; BLOCK];
    impulse[0] = 1.0;
    let silence = vec![0.0f32; BLOCK];
    let first = drive_input(
        node,
        &env(0),
        or_empty(Planes::new(&vec![&impulse[..]; width])),
        &[],
        &[],
    );
    // What the executor would park on: the narrowed figure.
    let tail = first
        .reported_tail
        .and_then(|r| crate::node::narrow_tail(shape.tail, r))
        .unwrap_or(shape.tail);
    let Some(tail) = tail.samples() else {
        return;
    };
    let silent_from = latency + tail.get() + 1;
    assert!(
        silent_from <= MAX_CHECKED_TAIL,
        "a tail of {silent_from} frames is too long to check here"
    );
    let mut audio = first.audio;
    let mut frame = BLOCK as u64;
    while audio.first().is_some_and(|c| c.len() < silent_from + BLOCK) {
        let next = drive_input(
            node,
            &env(frame),
            or_empty(Planes::new(&vec![&silence[..]; width])),
            &[],
            &[],
        );
        for (all, more) in audio.iter_mut().zip(next.audio) {
            all.extend(more);
        }
        frame += BLOCK as u64;
    }
    for (c, ch) in audio.iter().enumerate() {
        if let Some(i) = ch[silent_from..].iter().position(|&x| x != 0.0) {
            panic!(
                "channel {c} sounds at frame {} after an impulse, past its tail of \
                 {tail:?} and latency {latency}: the reported tail is too short",
                silent_from + i
            );
        }
    }
}

/// The longest tail, with its latency, [`assert_declared_tail`] renders.
pub const MAX_CHECKED_TAIL: usize = 1 << 20;

/// `node`, prepared at `rate` for blocks of up to `max_block` frames: the
/// state a graph hands [`drive`] a node in.
pub fn prepared<N: Node>(mut node: N, rate: SampleRate, max_block: usize) -> N {
    node.prepare(&Prepare::new(rate, MaxBlock::of(Samples::new(max_block))));
    node
}

/// Checks a [`ParamNode`]'s fork, the promise [`param_parts`](crate::param_parts)
/// relies on: a fork of `node` starts from the values last **set** through
/// its [`ParamSet`](crate::ParamSet) (the cells hold them: a modulation lane
/// sends to the port and never writes a cell), and shares no
/// cell with the live node in either direction.
///
/// What this catches in a node: a param missing from
/// [`ParamNode::param_set`] (a host cannot reach it by address), a
/// [`ParamNode::fork_fresh`] that starts its cells from the constructor's
/// defaults instead of the values set, and a cell it forgot to detach (a
/// fork that follows the live knob, or moves it).
///
/// # Panics
///
/// On the first broken promise, naming the param. Also if the node's set is
/// empty: a node with no params has nothing to check here.
pub fn assert_param_fork<N: ParamNode + Clone>(node: N) {
    let fork = ParamFork::new(&node);
    let live = fork.params().clone();
    let params: Vec<UnitAddr> = live.params().collect();
    assert!(!params.is_empty(), "a ParamNode with no params");
    // A distinct value per param, away from anything a constructor picks, and
    // inside every range-checked unit's range (a `Bipolar` holds `-1..=1`).
    // Each is an `f32` figure, which an `f32` unit's cell holds whole.
    let set = |i: usize| f64::from(0.37f32 + i as f32 * 0.11);
    let moved = |i: usize| f64::from(0.37f32 + i as f32 * 0.11 + 7.0);
    for (i, &p) in params.iter().enumerate() {
        live.set(p, set(i));
    }
    let forked = fork.fork_node();
    let fresh = forked.param_set();
    for (i, &p) in params.iter().enumerate() {
        assert_eq!(
            fresh.get(p),
            Some(set(i)),
            "{p:?}: a fork starts from the value set, not the constructor's"
        );
    }
    for (i, &p) in params.iter().enumerate() {
        live.set(p, moved(i));
        assert_eq!(
            fresh.get(p),
            Some(set(i)),
            "{p:?}: a live write reached the fork (a cell fork_fresh did not detach)"
        );
        fresh.set(p, -1.0);
        assert_eq!(
            live.get(p),
            Some(moved(i)),
            "{p:?}: a write to the fork reached the live node"
        );
    }
}
