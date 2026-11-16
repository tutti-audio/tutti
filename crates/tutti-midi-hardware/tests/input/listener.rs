//! The graph's side of the input, played on the wall clock.
//!
//! A port's events reach the graph through an event inlet, each on its frame.
//! A loopback test has no engine, so [`Listener`] is one: it adds the inlets of the
//! ports opened, writes the block clock the way the engine does (the OS time a
//! block begins and its frame) and renders blocks as real time passes, logging
//! what the inlets emit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_fixtures::{BlockClock, EventRig};
use tutti_graph::{EventEdge, EventIn, EventInletNode, EventOut, NodeKey};
use tutti_midi_runtime::{InputInlets, MidiIngestNode};
use tutti_types::{Frame, SampleRate};
use tutti_ump::Ump;

/// How long [`Listener::hear`] plays on after the events it waited for.
const SETTLE: Duration = Duration::from_millis(100);

const RATE: f64 = 44_100.0;
const BLOCK: usize = 64;
const SINK: NodeKey = EventRig::SINK;
const INGEST: NodeKey = NodeKey(2);

pub struct Listener {
    rig: EventRig,
    clock: BlockClock,
    origin: Instant,
    frames: u64,
    /// Where an inlet is wired: the sink, or an ingest node in front of it.
    entry: NodeKey,
}

impl Listener {
    /// A graph over `inlets`, whose clock reads the wall time now at frame 0.
    pub fn over(inlets: &Arc<InputInlets>) -> Self {
        Self::build(inlets, false)
    }

    /// As [`over`](Self::over), with the engine's ingest node between the
    /// inlets and the log: a MIDI 1.0 message arrives widened.
    pub fn over_ingest(inlets: &Arc<InputInlets>) -> Self {
        Self::build(inlets, true)
    }

    fn build(inlets: &Arc<InputInlets>, ingest: bool) -> Self {
        // The sink hears the one inlet's event input, or, behind the ingest
        // node, each output port of its first Group.
        let ports = tutti_midi_runtime::MidiInputPorts::FIRST_GROUP.count();
        let slots = if ingest { ports } else { 1 };
        let mut rig = EventRig::new(RATE, BLOCK, slots as u16);
        if ingest {
            rig.editor.insert(INGEST, "ingest", MidiIngestNode::new());
            for port in 0..ports as u16 {
                rig.feed(INGEST, port, port);
            }
        }
        rig.commit();
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        Self {
            rig,
            clock,
            origin: Instant::now(),
            frames: 0,
            entry: if ingest { INGEST } else { SINK },
        }
    }

    /// Adds the inlet of a port that opened and wires it to the entry's one
    /// event input.
    pub fn attach(&mut self, inlet: EventInletNode) {
        let key = NodeKey(100);
        self.rig.editor.insert(key, "inlet", inlet);
        self.rig.editor.spec_mut().connect_events(
            EventIn {
                node: self.entry,
                port: 0,
            },
            EventEdge::Direct(EventOut { node: key, port: 0 }),
        );
        self.rig.commit();
    }

    /// Renders the blocks the wall clock has reached.
    fn play(&mut self) {
        let due = (self.origin.elapsed().as_secs_f64() * RATE) as u64;
        while self.frames + BLOCK as u64 <= due {
            let start = self.origin + Duration::from_secs_f64(self.frames as f64 / RATE);
            self.clock
                .write(start, Frame::new(self.frames), SampleRate::of(RATE));
            self.rig.block(BLOCK);
            self.frames += BLOCK as u64;
        }
    }

    /// Plays until `count` events have arrived or `patience` has passed, then
    /// for [`SETTLE`] more, so that an event beyond `count` is heard too, and
    /// returns what arrived. The wait is for the events, not a span guessed to
    /// be long enough for a loaded machine.
    pub fn hear(&mut self, count: usize, patience: Duration) -> Vec<Ump> {
        let end = Instant::now() + patience;
        while self.rig.seen.lock().unwrap().len() < count && Instant::now() < end {
            self.play();
            std::thread::sleep(Duration::from_millis(1));
        }
        self.listen(SETTLE)
    }

    /// Plays for `span` of real time and returns what arrived.
    pub fn listen(&mut self, span: Duration) -> Vec<Ump> {
        let end = Instant::now() + span;
        while Instant::now() < end {
            self.play();
            std::thread::sleep(Duration::from_millis(1));
        }
        self.rig.take().into_iter().map(|(_, _, ump)| ump).collect()
    }
}
