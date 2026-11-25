//! The synth declares a cost class from its oscillator lanes: `Moderate` up to sixteen
//! (`polyphony` × unison), `Heavy` past them. Declaring one changes only the parallel schedule:
//! sibling synths pack into fewer tasks than undeclared ones, and the render on a pool is bit
//! for bit the one-participant one.

use tutti_graph::testing::WireDeclared;

use std::num::NonZeroUsize;
use std::sync::Arc;
use tutti_buffers::MaxBlock;
use tutti_fixtures::signal::plane_bits as bits;
use tutti_graph::EdgeId;

use tutti_graph::NodeKey;
use tutti_graph::WorkerPool;
use tutti_graph::{
    Cost, Cx, Editor, EventEdge, EventInputPort, EventOutputPort, Executor, Io, Node, Prepare,
    Shape, Status, TransportPosition, Unforkable,
};
use tutti_graph::{OutputPort, PortSource};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_polysynth::{OscillatorType, PolySynth, SynthConfig, UnisonConfig, UnisonVoices};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Beat, Bpm, SampleRate, Samples};

const SR: f64 = 48_000.0;
const BPM: f64 = 120.0;
/// Small enough that a `Moderate` stereo synth is under the packing grain.
const MAX_BLOCK: usize = 64;
/// Synths at the default eight voices (`Moderate`).
const LIGHT: u64 = 8;
/// Synths at 32 voices (`Heavy`), so the plan still spreads.
const HEAVY: u64 = 2;

fn synth(max_voices: usize, unison: u8) -> PolySynth<f32> {
    let config = SynthConfig::new()
        .oscillator(OscillatorType::Saw)
        .polyphony(NonZeroUsize::new(max_voices).expect("at least one voice"));
    PolySynth::<f32>::new(if unison > 1 {
        config.unison(
            UnisonConfig::new().voice_count(UnisonVoices::new(unison).expect("a unison count")),
        )
    } else {
        config
    })
}

/// `node` as a node that declares nothing: the cost model's default.
struct Undeclared<N>(N);

impl<N: Node> Node for Undeclared<N> {
    fn shape(&self) -> Shape {
        self.0.shape().cost(Cost::Heavy)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.0.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.process(cx, io)
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

/// Two notes for synth `k`, in the first hundredth of a beat.
fn notes(k: u64) -> Vec<TimedMidiEvent> {
    (0..2)
        .map(|i| {
            TimedMidiEvent::new(
                Beat::new(0.001 * (k + 3 * i) as f64),
                Message::midi2(
                    MidiChannel::FIRST,
                    Voice2::note_on(
                        NoteNumber::of(48 + (k * 5 + i * 7) as u8),
                        NoteVelocity::MAX,
                    ),
                ),
            )
            .unwrap()
        })
        .collect()
}

/// [`LIGHT`] eight-voice synths and [`HEAVY`] 32-voice ones, each played
/// by a clip of its own, summed by the graph (a fan-in onto each of two main outputs);
/// on a pool of `workers` when more than one. `declared`: the eight-voice
/// synths as shipped, or wrapped to declare nothing.
fn graph(workers: usize, declared: bool) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(MAX_BLOCK)),
    ));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(
            WorkerPool::builder()
                .num_threads(workers)
                .build()
                .expect("a worker pool"),
        )));
    }
    let sources = LIGHT + HEAVY;
    // Two silent main outputs, which the fan-in below fills.
    ed.wire_declared_outputs([PortSource::Zero; 2]).unwrap();
    for k in 0..sources {
        let (clip, key) = (NodeKey(100 + k), NodeKey(k + 1));
        ed.insert(clip, "clip", MidiClipNode::new(notes(k)));
        match (k < LIGHT, declared) {
            (true, true) => {
                ed.insert(key, "synth", synth(8, 1));
            }
            (true, false) => {
                ed.insert(key, "synth", Unforkable(Undeclared(synth(8, 1))));
            }
            (false, _) => {
                ed.insert(key, "synth", synth(32, 1));
            }
        }
        ed.connect_events(
            EventInputPort::new(key, 0),
            EventEdge::Direct(EventOutputPort::new(clip, 0)),
        )
        .unwrap();
        // The bus is the graph's fan-in: each output channel holds every
        // synth's matching port, and the graph sums them.
        for c in 0..2u16 {
            let from = PortSource::Node(OutputPort::new(key, c));
            ed.connect_edge(EdgeId::into_output((c as usize) as u16, from))
                .unwrap();
        }
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

/// Blocks from one frame to the maximum, ragged, rolling from beat 0.
fn render(exec: &mut Executor) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); 2];
    let mut frame = 0usize;
    for i in 0..60 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let mut chunk = vec![vec![0.0f32; n]; 2];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let beat = frame as f64 * BPM / 60.0 / SR;
            let t = TransportPosition::new(true, Bpm::of(BPM), Beat::new(beat), None);
            exec.process(n, &t, &[], &mut outs);
        }
        for (a, c) in all.iter_mut().zip(chunk) {
            a.extend(c);
        }
        frame += n;
    }
    all
}

/// Sixteen lanes and fewer are `Moderate`, more are `Heavy`, whether the
/// lanes are voices or unison sub-voices.
#[test]
fn the_class_follows_the_lane_count() {
    assert_eq!(synth(8, 1).shape().cost, Cost::Moderate);
    assert_eq!(synth(16, 1).shape().cost, Cost::Moderate);
    assert_eq!(synth(8, 2).shape().cost, Cost::Moderate);
    assert_eq!(synth(17, 1).shape().cost, Cost::Heavy);
    assert_eq!(synth(8, 4).shape().cost, Cost::Heavy);
    assert_eq!(synth(32, 1).shape().cost, Cost::Heavy);
}

/// Eight `Moderate` synths beside two `Heavy` ones: the moderate ones (each
/// fused with its clip) pack into fewer tasks than the same synths
/// declaring nothing, the plan still spreads, and on a pool of two and of
/// four the render is bit for bit the one-participant one.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn moderate_synths_pack_and_render_bit_identically() {
    let declared = graph(4, true);
    let undeclared = graph(4, false);
    let (plan, bare) = (
        declared.plan().expect("a plan"),
        undeclared.plan().expect("a plan"),
    );
    assert!(
        plan.tasks().len() < bare.tasks().len(),
        "declared {} tasks, undeclared {}",
        plan.tasks().len(),
        bare.tasks().len()
    );
    // Several tasks: every block below spreads on a pool, so the comparison
    // is never one participant against itself.
    assert!(plan.tasks().len() > 1, "{:?}", plan.tasks());

    let serial = render(&mut graph(1, true));
    assert!(serial.iter().all(|c| c.iter().any(|&x| x != 0.0)));
    for workers in [2, 4] {
        let mut exec = graph(workers, true);
        assert!(exec.is_parallel());
        assert_eq!(bits(&serial), bits(&render(&mut exec)), "{workers} workers");
    }
}
