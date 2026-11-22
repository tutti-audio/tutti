//! [`Sink`] and [`EventRig`]: a graph whose one node logs the MIDI its event
//! inputs receive, for the tests of what feeds a graph's event inputs.

use std::sync::{Arc, Mutex};
use tutti_platform::Instant;

use crate::Record;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{
    Cx, Editor, EventEdge, EventInputPort, EventKind, EventOutputPort, Executor, ForkByClone, Io,
    Node, NodeKey, OutputPort, PortSource, Prepare, Shape, Status, TransportPosition,
};
use tutti_ump::Ump;
use tutti_units::{Frame, Moment, SampleRate, Samples};

/// `(event input, absolute frame, event)` of every MIDI event a [`Sink`]
/// received, in the order its inputs and frames were processed.
pub type Seen = Record<(u16, u64, Ump)>;

/// The frame and event of each entry of `seen`, the input dropped: what a sink of
/// one input logged.
///
/// # Panics
///
/// Panics when the log's lock is poisoned.
#[must_use]
pub fn heard(seen: &Seen) -> Vec<(u64, Ump)> {
    seen.lock()
        .expect("the log is not poisoned")
        .iter()
        .map(|&(_, frame, ump)| (frame, ump))
        .collect()
}

/// What a [`Sink`] renders: a silent mono output, or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// A silent mono output: the graph is rendered into one buffer.
    Mono,
    /// No output: the graph is rendered into none.
    None,
}

/// A node with `ports` event inputs that logs the MIDI on each input, and a
/// silent mono output unless built [`Output::None`]. A fork of it (`ForkByClone`)
/// logs into the same list.
#[derive(Clone, Debug)]
pub struct Sink {
    ports: u16,
    seen: Seen,
    output: Output,
}

impl Sink {
    /// A sink of `ports` event inputs logging into `seen`, with `output`.
    #[must_use]
    pub fn new(ports: u16, seen: &Seen, output: Output) -> Self {
        Self {
            ports,
            seen: seen.clone(),
            output,
        }
    }
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        match self.output {
            Output::Mono => {
                Shape::source_unassigned(ChannelLayout::MONO).with_events(self.ports, 0)
            }
            Output::None => Shape::EMPTY.with_events(self.ports, 0),
        }
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().expect("the log is not poisoned");
        for port in 0..self.ports {
            for event in io.events(port) {
                if let EventKind::Midi(ump) = event.kind {
                    seen.push((
                        port,
                        cx.env.frame.get() + u64::from(event.offset.get()),
                        ump,
                    ));
                }
            }
        }
        match self.output {
            Output::Mono => {
                io.output(0).fill(0.0);
                Status::Modified
            }
            Output::None => Status::Silent,
        }
    }

    fn reset(&mut self) {}
}

/// A graph with a [`Sink`] at [`SINK`](Self::SINK), an editor to build what
/// feeds it and an executor to render it.
#[derive(Debug)]
pub struct EventRig {
    /// The graph's editor: insert the sources, then [`feed`](Self::feed) and commit.
    pub editor: Editor,
    /// The graph's executor.
    pub exec: Executor,
    /// What the sink has received.
    pub seen: Seen,
    output: Output,
}

impl EventRig {
    /// Where the sink is.
    pub const SINK: NodeKey = NodeKey(u64::MAX);

    /// A graph at `rate`, blocks of at most `max_block` frames, whose sink has
    /// `ports` event inputs and a silent mono output. Nothing feeds it and it is
    /// not committed yet.
    #[must_use]
    pub fn new(rate: f64, max_block: usize, ports: u16) -> Self {
        Self::with_output(rate, max_block, ports, Output::Mono)
    }

    /// As [`new`](Self::new), the sink with no output: render with
    /// `exec.process(.., &mut [])`.
    #[must_use]
    pub fn events_only(rate: f64, max_block: usize, ports: u16) -> Self {
        Self::with_output(rate, max_block, ports, Output::None)
    }

    fn with_output(rate: f64, max_block: usize, ports: u16, output: Output) -> Self {
        let (mut editor, exec) = Editor::new(Prepare::new(
            SampleRate::of(rate),
            MaxBlock::of(Samples::new(max_block)),
        ));
        let seen = Seen::new();
        editor.insert(
            Self::SINK,
            "sink",
            ForkByClone(Sink::new(ports, &seen, output)),
        );
        if output == Output::Mono {
            editor
                .spec_mut()
                .topology
                .set_outputs(vec![PortSource::Node(OutputPort {
                    node: Self::SINK,
                    port: 0,
                })]);
        }
        Self {
            editor,
            exec,
            seen,
            output,
        }
    }

    /// Feeds event input `to` of the sink from event output `from_port` of `from`.
    pub fn feed(&mut self, from: NodeKey, from_port: u16, to: u16) {
        self.editor.spec_mut().connect_events(
            EventInputPort {
                node: Self::SINK,
                port: to,
            },
            EventEdge::Direct(EventOutputPort {
                node: from,
                port: from_port,
            }),
        );
    }

    /// Applies what the editor holds.
    ///
    /// # Panics
    ///
    /// When the graph does not commit.
    pub fn commit(&mut self) {
        self.editor.commit().expect("commits");
    }

    /// Renders `frames` frames as one block, the transport stopped.
    pub fn block(&mut self, frames: usize) {
        let mut out = vec![0.0f32; frames];
        let transport = TransportPosition::default();
        match self.output {
            Output::Mono => self
                .exec
                .process(frames, &transport, &[], &mut [&mut out[..]]),
            Output::None => self.exec.process(frames, &transport, &[], &mut []),
        }
    }

    /// Everything the sink has received, taken.
    ///
    /// # Panics
    ///
    /// Panics when the log's lock is poisoned.
    #[must_use]
    pub fn take(&self) -> Vec<(u16, u64, Ump)> {
        let mut log = self.seen.lock().expect("the log is not poisoned");
        let all = log.to_vec();
        log.clear();
        all
    }
}

/// What a block clock reads: the OS time a block begins and its frame.
type Reading = Option<(Instant, Moment)>;

/// A block clock a test writes by hand, as the engine does each render call.
#[derive(Debug)]
pub struct BlockClock(Arc<Mutex<Reading>>);

impl Default for BlockClock {
    fn default() -> Self {
        let clock = Self(Arc::default());
        // On macOS std's `Mutex` boxes its pthread mutex on the first `lock`,
        // so a first read from the driver thread would allocate there. The
        // engine's own clock is lock-free; this stand-in takes its lock once
        // here, on the control side, where a real one is built.
        drop(clock.0.lock().expect("the clock is not poisoned"));
        clock
    }
}

impl BlockClock {
    /// A clock that has not been written.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A reader of the clock, as a device inlet takes it.
    ///
    /// # Panics
    ///
    /// The reader panics when the clock's lock is poisoned.
    #[must_use]
    pub fn reader(&self) -> Arc<dyn Fn() -> Option<(Instant, Moment)> + Send + Sync> {
        let cell = Arc::clone(&self.0);
        Arc::new(move || *cell.lock().expect("the clock is not poisoned"))
    }

    /// Says that OS time `os` is frame `frame` at `rate`.
    ///
    /// # Panics
    ///
    /// Panics when the clock's lock is poisoned.
    pub fn write(&mut self, os: Instant, frame: Frame, rate: SampleRate) {
        *self.0.lock().expect("the clock is not poisoned") = Some((os, Moment::new(frame, rate)));
    }
}
