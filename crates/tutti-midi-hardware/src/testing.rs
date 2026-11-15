//! What this crate's tests share: the inlets a port list is built over, and a
//! graph that plays the audio thread, so a test sees an event where the graph
//! would: on its frame, after the inlet has held it for its block.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tutti_types::MaxBlock;

use tutti_graph::EventInletNode;
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, Io, Node, NodeKey, OutPort,
    Prepare, Shape, Source, Status, TransportPosition, Unforkable,
};
use tutti_midi_runtime::InputInlets;
use tutti_types::{Frame, SampleRate, Samples};
use tutti_ump::Ump;

use crate::HardwareMidiInputs;

/// A block clock a test writes by hand, as the engine does each render call.
pub(crate) struct BlockClock(
    std::sync::Arc<std::sync::Mutex<Option<(std::time::Instant, tutti_types::Moment)>>>,
);

impl BlockClock {
    fn new() -> Self {
        Self(Default::default())
    }

    fn reader(&self) -> tutti_midi_runtime::ArrivalClock {
        let cell = std::sync::Arc::clone(&self.0);
        std::sync::Arc::new(move || *cell.lock().unwrap())
    }

    fn write(
        &mut self,
        os: std::time::Instant,
        frame: tutti_types::Frame,
        rate: tutti_types::SampleRate,
    ) {
        *self.0.lock().unwrap() = Some((os, tutti_types::Moment::new(frame, rate)));
    }
}

pub(crate) const RATE: f64 = 44_100.0;

/// A list of ports over inlets of 64 events, with no lead.
pub(crate) fn ports() -> Arc<HardwareMidiInputs> {
    Arc::new(HardwareMidiInputs::new(InputInlets::new(
        tutti_graph::EventCapacity::of(64),
        Duration::ZERO,
    )))
}

/// `(slot, absolute frame, event)` of every event the sink received.
pub(crate) type Seen = Arc<Mutex<Vec<(usize, u64, Ump)>>>;

/// Logs the MIDI on each of its event inputs; a silent mono output.
struct Sink {
    slots: u16,
    seen: Seen,
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(tutti_types::ChannelLayout::MONO).with_events(self.slots, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for slot in 0..usize::from(self.slots) {
            for e in io.events(slot) {
                if let EventKind::Midi(ump) = e.kind {
                    seen.push((slot, cx.env.frame.get() + u64::from(e.offset.get()), ump));
                }
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The graph's owner and its audio thread in one: it adds the inlets it is
/// handed ([`attach`](Self::attach)) as a session's `Poll` phase does, writes the
/// block clock and renders blocks of 64 frames.
pub(crate) struct Rig {
    pub(crate) seen: Seen,
    clock: BlockClock,
    /// When frame 0 began.
    pub(crate) origin: Instant,
    editor: Editor,
    exec: Executor,
    pub(crate) frames: u64,
    /// How many inlets are attached: the sink input the next one feeds.
    attached: usize,
}

impl Rig {
    pub(crate) const SINK: NodeKey = NodeKey(1);
    pub(crate) const BLOCK: usize = 64;
    /// How many inlets the sink has an event input for.
    pub(crate) const INPUTS: u16 = 4;

    /// A rig over `inlets`, whose clock reads `origin` at frame 0.
    pub(crate) fn over(inlets: &Arc<InputInlets>, origin: Instant) -> Self {
        let (mut editor, exec) = Editor::new(Prepare::new(
            SampleRate::of(RATE),
            MaxBlock::of(Samples(Self::BLOCK)),
        ));
        let seen = Seen::default();
        editor.insert(
            Self::SINK,
            "sink",
            Unforkable(Sink {
                slots: Self::INPUTS,
                seen: Arc::clone(&seen),
            }),
        );
        editor
            .spec_mut()
            .topology
            .set_outputs(vec![Source::Node(OutPort {
                node: Self::SINK,
                port: 0,
            })]);
        editor.commit().expect("commits");
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        Self {
            seen,
            clock,
            origin,
            editor,
            exec,
            frames: 0,
            attached: 0,
        }
    }

    /// Adds `inlet` to the graph and wires it to the sink's next event input,
    /// whose index (the slot of what the sink logs) it returns.
    pub(crate) fn attach(&mut self, inlet: EventInletNode) -> usize {
        let input = self.attached;
        assert!(
            input < usize::from(Self::INPUTS),
            "the sink has no input left"
        );
        self.attached += 1;
        let key = NodeKey(100 + input as u64);
        self.editor.insert(key, "inlet", inlet);
        self.editor.spec_mut().connect_events(
            EventIn {
                node: Self::SINK,
                port: input as u16,
            },
            EventEdge::Direct(EventOut { node: key, port: 0 }),
        );
        self.editor.commit().expect("commits");
        input
    }

    /// The OS time at which graph frame `frame` begins.
    pub(crate) fn time_of(&self, frame: u64) -> Instant {
        self.origin + Duration::from_secs_f64(frame as f64 / RATE)
    }

    /// Renders `blocks` blocks, each preceded by its clock reading as the
    /// engine writes it.
    pub(crate) fn run(&mut self, blocks: usize) {
        for _ in 0..blocks {
            self.clock.write(
                self.time_of(self.frames),
                Frame::new(self.frames),
                SampleRate::of(RATE),
            );
            let mut out = vec![0.0f32; Self::BLOCK];
            self.exec.process(
                Self::BLOCK,
                &TransportPosition::default(),
                &[],
                &mut [&mut out[..]],
            );
            self.frames += Self::BLOCK as u64;
        }
    }

    /// Everything the sink has received, taken.
    pub(crate) fn take(&self) -> Vec<(usize, u64, Ump)> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}
