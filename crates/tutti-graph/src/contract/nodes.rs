//! The harness's own nodes: sources, delays and sinks a row builds its graph
//! around.

use crate::{InEvents, OutEvents};
use tutti_buffers::ChannelLayout;
use tutti_rt::{Overflow, RingReader, RingWriter, ring};
use tutti_units::{Frame, Latency, Samples, UnitAddr};

use crate::event::types::{Event, EventKind};
use crate::node::io::Io;
use crate::node::param::ParamInput;
use crate::node::param_event::{ParamEvent, ParamSegment};
use crate::node::shape::Shape;
use crate::node::{Cx, Node, Prepare, Resolution, Status};

/// The param row's node: it declares one drivable param, whose base is
/// 0, and writes that param's value at every frame to its one output — 0
/// while it reads its base. Its one audio input (for the PDC path's latent
/// sibling) is ignored.
#[derive(Clone, Debug)]
pub struct ParamEcho {
    param: UnitAddr,
}

impl ParamEcho {
    /// An echo of `param`.
    #[must_use]
    pub fn new(param: impl Into<UnitAddr>) -> Self {
        Self {
            param: param.into(),
        }
    }
}

impl Node for ParamEcho {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO).param_addrs(&[self.param])
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0) {
            ParamInput::Base => io.output(0).fill(0.0),
            ParamInput::Frames { narrow: v, .. } | ParamInput::Smoothing { narrow: v, .. } => {
                io.output(0).copy_from_slice(v);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}

    fn param_base(&self, port: usize) -> Option<f64> {
        (port == 0).then_some(0.0)
    }
}

/// An event-driven impulse: every event on its one event input adds one
/// sample of `1.0` to its one audio output, [`latency`](Self::new) frames
/// after the frame the event takes effect on. *Adds*: two events on one
/// frame are one sample of `2.0`, so a duplicated delivery is visible.
///
/// Written against [`Io::sub_blocks`](crate::Io::sub_blocks), so at
/// [`Resolution::Sample`] (the default) it is sample-accurate by
/// construction: this is the graph-level row, and the source the engine-level
/// rows play. [`with_resolution`](Self::with_resolution) makes it as coarse
/// as it declares:
///
/// - `Frames(n)`: an event takes effect at the start of the node's **own**
///   next `n`-frame chunk (a grid of its own, offset from the graph's
///   blocks, as an engine that renders in fixed chunks would be) — up to
///   `n - 1` frames late;
/// - `Block`: at its block's first frame.
#[derive(Clone, Debug)]
pub struct ImpulseSource {
    port: InEvents,
    latency: Latency,
    resolution: Resolution,
    /// Absolute frames of impulses still to come; a fixed array, so the
    /// audio thread never allocates.
    pending: [u64; PULSE_PENDING],
    len: usize,
}

/// How many impulses a [`ImpulseSource`] holds in flight.
const PULSE_PENDING: usize = 16;

/// Where a `Frames(n)` [`ImpulseSource`]'s own chunk grid starts: frame 3, so it
/// lines up with no block boundary a schedule of the usual sizes makes.
const PULSE_GRID_PHASE: u64 = 3;

impl ImpulseSource {
    /// A pulse delayed by `latency`, declared as its [`Shape::latency`].
    #[must_use]
    pub fn new(latency: Latency) -> Self {
        Self {
            port: Self::IN,
            latency,
            resolution: Resolution::Sample,
            pending: [0; PULSE_PENDING],
            len: 0,
        }
    }

    /// This pulse, honouring offsets only as finely as `resolution` (see
    /// the type docs), and declaring so.
    #[must_use]
    pub fn with_resolution(mut self, resolution: Resolution) -> Self {
        self.resolution = resolution;
        self
    }

    const BASE: Shape = Shape::source_unassigned(ChannelLayout::MONO).events(1, 0);
    const IN: InEvents = Self::BASE.event_input(0).expect("declared");

    fn declared(latency: Latency, resolution: Resolution) -> Shape {
        Self::BASE
            .latency(latency)
            .tail(tutti_units::Tail::Unknown)
            .event_resolution(resolution)
    }
}

impl Node for ImpulseSource {
    fn shape(&self) -> Shape {
        Self::declared(self.latency, self.resolution)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block = cx.env.frame.get();
        for (range, events) in io.sub_blocks(self.port) {
            let t = block + range.start as u64;
            let at = match self.resolution {
                Resolution::Sample => t,
                Resolution::Frames(n) => {
                    let n = u64::from(n.max(1));
                    t + (n - (t + PULSE_GRID_PHASE) % n) % n
                }
                Resolution::Block => block,
            };
            for _ in events {
                if self.len < PULSE_PENDING {
                    self.pending[self.len] = at + self.latency.samples().get() as u64;
                    self.len += 1;
                }
            }
        }
        let frames = io.frames() as u64;
        let out = io.output(0);
        out.fill(0.0);
        let mut i = 0;
        while i < self.len {
            let t = self.pending[i];
            if t < block + frames {
                // An impulse already past (never, for a node called every
                // block) is dropped rather than written late.
                if t >= block {
                    out[(t - block) as usize] += 1.0;
                }
                self.len -= 1;
                self.pending[i] = self.pending[self.len];
            } else {
                i += 1;
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.len = 0;
    }
}

/// An audio delay that declares its delay as processing latency — a
/// lookahead with nothing to look ahead for. The graph-level audio-impulse row.
#[derive(Clone, Debug)]
pub struct Lookahead {
    latency: Latency,
    ring: Vec<f32>,
    pos: usize,
}

impl Lookahead {
    /// A delay of `latency`, declared as such.
    #[must_use]
    pub fn new(latency: Latency) -> Self {
        Self {
            latency,
            ring: vec![0.0; latency.samples().get()],
            pos: 0,
        }
    }
}

impl Node for Lookahead {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .latency(self.latency)
            // Pure delay: nothing rings once the latency is compensated.
            .tail(tutti_units::Tail::None)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = self.ring.len();
        if n == 0 {
            return Status::Bypass;
        }
        for i in 0..io.frames() {
            let x = io.input(0)[i];
            io.output(0)[i] = std::mem::replace(&mut self.ring[self.pos], x);
            self.pos = (self.pos + 1) % n;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
    }
}

/// A latent sibling: declares `latency`, outputs silence on one audio
/// output and nothing on one event output. Wired beside a path, it raises
/// the arrival of whatever it merges into without adding a signal.
#[derive(Clone, Copy, Debug)]
pub struct Latent {
    latency: Latency,
}

impl Latent {
    /// A silent node declaring `latency`.
    #[must_use]
    pub fn new(latency: Latency) -> Self {
        Self { latency }
    }
}

impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .events(0, 1)
            .latency(self.latency)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }

    fn reset(&mut self) {}
}

/// The events a [`Recorder`] saw: `(absolute frame, event)`, in the order the
/// node was handed them. The reader end; a push past [`EVENT_LOG_CAPACITY`]
/// is counted lost, and the reading assert fails on any loss.
pub(super) type EventLog = RingReader<(u64, EventKind)>;

/// The events a [`Recorder`] holds: far more than an onset-release row sends.
const EVENT_LOG_CAPACITY: usize = 1024;

/// A wrapper around the node under test that records what its event input
/// `port` is handed, then delegates every call. It is in the harness so that
/// a path can assert what reached a node without a hook inside the node.
pub(super) struct Recorder {
    inner: Box<dyn Node>,
    port: InEvents,
    log: RingWriter<(u64, EventKind)>,
}

impl Recorder {
    pub(super) fn wrap(inner: Box<dyn Node>, port: InEvents) -> (Self, EventLog) {
        let (log, reader) = ring(EVENT_LOG_CAPACITY, Overflow::DropNewest);
        (Self { inner, port, log }, reader)
    }
}

impl Node for Recorder {
    fn shape(&self) -> Shape {
        self.inner.shape()
    }

    fn bind_wake(&mut self, wake: crate::Wake) {
        self.inner.bind_wake(wake);
    }

    fn prepare(&mut self, p: &Prepare) {
        self.inner.prepare(p);
    }

    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let block = cx.env.frame.get();
        self.log.push_iter(
            io.events(self.port)
                .iter()
                .map(|e| (block + e.offset.index() as u64, e.kind)),
        );
        self.inner.process(cx, io)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    fn param_base(&self, port: usize) -> Option<f64> {
        self.inner.param_base(port)
    }
}

/// Sends one event, `kind`, on its one event output at absolute frame `at`.
#[derive(Clone, Copy, Debug)]
pub struct Emitter {
    port: OutEvents,
    at: Frame,
    kind: EventKind,
}

impl Emitter {
    /// An emitter armed for `at`.
    #[must_use]
    pub fn new(at: Frame, kind: EventKind) -> Self {
        Self {
            port: Self::OUT,
            at,
            kind,
        }
    }

    const SHAPE: Shape = Shape::EMPTY.events(0, 1);
    const OUT: OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl Node for Emitter {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(offset) = cx.env.offset_of(self.at) {
            io.event_out(self.port)
                .push(Event {
                    offset,
                    kind: self.kind,
                })
                .expect("one event fits");
        }
        Status::Silent
    }

    fn reset(&mut self) {}
}

/// Drives one param for one frame: on its one event output, a step to
/// `amplitude` at absolute frame `at` and a step back to 0 at `at + 1`,
/// each addressed to `param` ([`ParamSegment::unit`], zero length). Each
/// step is sent in the block holding its frame, so a re-insert armed alike
/// sends only the steps still ahead.
#[derive(Clone, Copy, Debug)]
pub struct ParamPulse {
    port: OutEvents,
    at: Frame,
    param: UnitAddr,
    amplitude: f64,
}

impl ParamPulse {
    /// A pulse of `amplitude` on `param` at `at`.
    #[must_use]
    pub fn new(at: Frame, param: UnitAddr, amplitude: f64) -> Self {
        Self {
            port: Self::OUT,
            at,
            param,
            amplitude,
        }
    }

    const SHAPE: Shape = Shape::EMPTY.events(0, 1).param_events();
    const OUT: OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl Node for ParamPulse {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (frame, value) in [(self.at, self.amplitude), (self.at + Samples::new(1), 0.0)] {
            if let Some(offset) = cx.env.offset_of(frame) {
                let step = ParamSegment::unit(self.param, value, value, Samples::new(0));
                io.event_out(self.port)
                    .push_param(ParamEvent::segment(offset, step))
                    .expect("two events fit");
            }
        }
        Status::Silent
    }

    fn reset(&mut self) {}
}

/// Two audio inputs, summed into one output.
#[derive(Clone, Copy, Debug)]
pub struct Sum;

impl Node for Sum {
    fn shape(&self) -> Shape {
        Shape::buses(
            crate::Buses::unassigned(ChannelLayout::STEREO),
            crate::Buses::unassigned(ChannelLayout::MONO),
        )
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            let s = io.input(0)[i] + io.input(1)[i];
            io.output(0)[i] = s;
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}
