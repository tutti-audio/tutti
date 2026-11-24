//! A node behind a latent path reads the transport its input was rendered
//! at: `Cx::transport_at(offset)` is the transport `Cx::arrival` frames
//! earlier, through block boundaries, seeks, transport changes inside a
//! block, loop wraps, stops and ragged blocks.
//!
//! The truth is the blocks themselves: the transport at timeline frame `F`
//! is what `Env::transport_at` said for `F` in the block that held it. The
//! one exception is a frame older than every piece the executor's history
//! keeps (before the first block, past more than `TransportHistory::JUMPS`
//! pieces, and after a sample-rate change): the oldest piece kept, unmoved.
//! Where that boundary falls is modelled here from the history's documented
//! rules: a block's first frame or change is kept when its transport is not
//! the newest piece rolled forward (or the newest is a checkpoint's length
//! behind a block start), at most `JUMPS` are kept, the newest, and a rate
//! change clears them. The roll is the public `Env::transport_at`'s.

use tutti_graph::testing as common;

use std::collections::VecDeque;
use tutti_buffers::MaxBlock;

use common::prepare;
use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Env, Executor, Io, Node, Offset, Prepare, Shape, Status, TransportChanges,
    TransportHistory, TransportPosition, Unforkable,
};
use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
use tutti_music::LoopRange;
use tutti_units::{Beat, Bpm, Frame, Latency, SampleRate, Samples};

const MAX: usize = 64;
const LATENT: NodeKey = NodeKey(1);
const PROBE: NodeKey = NodeKey(2);
/// Frames per beat at 120 BPM and the test rate (48 kHz).
const FPB: f64 = 24_000.0;

/// A source declaring `lat` frames of latency: all it does is make the probe
/// after it arrive that late.
#[derive(Clone)]
struct Latent(usize);

impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).latency(Latency::new(Samples::new(self.0)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Writes, per frame, the transport its input was rendered at
/// (`Cx::transport_at`): the beat on output 0, and the tempo plus flags
/// (1000 playing, 2000 recording, 4000 looping) on output 1. Output 2 is
/// its arrival, so a test can tell the compile moved it.
#[derive(Clone)]
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::buses(
            tutti_graph::Buses::unassigned(ChannelLayout::MONO),
            tutti_graph::Buses::unassigned(ChannelLayout::from_count(3)),
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (i, k) in cx.env.offsets().enumerate() {
            let [beat, rest] = encode(&cx.transport_at(k));
            io.output(0)[i] = beat;
            io.output(1)[i] = rest;
            io.output(2)[i] = cx.arrival.samples().get() as f32;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn encode(t: &TransportPosition) -> [f32; 2] {
    let flags = 1000.0 * f32::from(u8::from(t.playing))
        + 2000.0 * f32::from(u8::from(t.recording()))
        + 4000.0 * f32::from(u8::from(t.looping.is_some()));
    [t.beat().get() as f32, t.tempo.get() as f32 + flags]
}

fn topology(ed: &mut Editor) {
    ed.connect_edge(tutti_graph::EdgeId {
        sink: tutti_graph::PortSink::Port(InputPort::new(PROBE, 0)),
        from: Edge::Direct(PortSource::Node(OutputPort::new(LATENT, 0))),
    })
    .expect("the edge wires");
    ed.set_outputs((0..3).map(|port| PortSource::Node(OutputPort::new(PROBE, port))))
        .expect("the sources exist");
}

struct Rig {
    ed: Editor,
    exec: Executor,
    /// Every block rendered, as the executor was handed it.
    played: Vec<Env>,
    /// The pieces the executor's history holds, by its documented rules:
    /// each piece's first frame, transport and rate.
    kept: VecDeque<(u64, TransportPosition, SampleRate)>,
    /// The rate blocks are rendered at.
    rate: SampleRate,
    /// Frames checked that were older than every piece kept, and after
    /// frame 0.
    clamped: usize,
}

impl Rig {
    fn new(lat: usize) -> Self {
        let (mut ed, exec) = Editor::new(prepare(MAX));
        ed.insert(LATENT, "latent", Unforkable(Latent(lat)));
        ed.insert(PROBE, "probe", Unforkable(Probe));
        topology(&mut ed);
        ed.commit().expect("commits");
        Self {
            ed,
            exec,
            played: Vec::new(),
            kept: VecDeque::new(),
            rate: prepare(MAX).sample_rate(),
            clamped: 0,
        }
    }

    /// The latent node's latency moves (a plugin's pipeline changing).
    fn set_latency(&mut self, lat: usize) {
        self.ed
            .set_latency(LATENT, Latency::new(Samples::new(lat)))
            .expect("the node exists");
        self.ed.commit().expect("recompiles");
    }

    /// Re-prepares at `rate`, with no block between the two commits.
    /// The ring is cleared: frames at the old rate name nothing at the new.
    fn reprepare(&mut self, rate: SampleRate) {
        let p = Prepare::new(rate, MaxBlock::of(Samples::new(MAX)));
        self.ed.reprepare(p).expect("reprepares");
        self.exec.apply_pending(); // checks the units out
        self.ed.collect(); // re-prepares them, sends them back
        self.exec.apply_pending(); // installs them
        self.ed.collect();
        self.rate = rate;
        self.kept.clear();
    }

    /// One block through the executor; returns its outputs.
    fn block(
        &mut self,
        n: usize,
        t: TransportPosition,
        changes: TransportChanges,
    ) -> [Vec<f32>; 3] {
        let env = Env {
            frame: self.exec.frame(),
            sample_rate: self.rate,
            block_len: Samples::new(n),
            transport: t,
            changes,
            play: tutti_graph::PlayClock::ZERO,
        };
        let mut a: [Vec<f32>; 3] = std::array::from_fn(|_| vec![0.0; n]);
        {
            let [a0, a1, a2] = &mut a;
            self.exec
                .process_with_changes(n, &t, &changes, &[], &mut [a0, a1, a2]);
        }
        self.ed.collect();
        self.played.push(env);
        for (start, piece) in env.segments() {
            let keep = match self.kept.back() {
                None => true,
                Some(&(f, t, rate)) => {
                    let gap = piece.frame.get() - f;
                    let span = Samples::new(usize::try_from(gap).unwrap());
                    // The newest piece rolled forward `gap` frames, through
                    // the public walk.
                    let rolled = Env {
                        frame: Frame::new(f),
                        sample_rate: rate,
                        block_len: span + Samples::new(1),
                        transport: t,
                        changes: TransportChanges::NONE,
                        play: tutti_graph::PlayClock::ZERO,
                    }
                    .transport_at(Offset::of(span, span + Samples::new(1)));
                    rate != env.sample_rate
                        || rolled != piece.transport
                        || (start == Offset::ZERO
                            && gap >= TransportHistory::checkpoint(env.sample_rate).get() as u64)
                }
            };
            if keep {
                self.kept
                    .push_back((piece.frame.get(), piece.transport, env.sample_rate));
                if self.kept.len() > TransportHistory::JUMPS {
                    self.kept.pop_front();
                }
            }
        }
        a
    }

    /// What the block holding timeline frame `f` said for it; older than
    /// every piece kept (or before frame 0), the oldest kept, unmoved.
    fn truth(&self, f: Option<u64>) -> TransportPosition {
        let (oldest, t, _) = *self.kept.front().expect("a block was observed");
        let Some(f) = f.filter(|&f| f >= oldest) else {
            return t;
        };
        let e = self
            .played
            .iter()
            .find(|e| e.offset_of(Frame::new(f)).is_some())
            .expect("a block played that frame");
        e.transport_at(e.offset_of(Frame::new(f)).expect("inside"))
    }

    /// Every frame of the block just rendered reads the transport `lat`
    /// frames earlier.
    fn check(&mut self, out: &[Vec<f32>; 3], lat: usize, what: &str) {
        let e = *self.played.last().expect("rendered");
        for (i, k) in e.offsets().enumerate() {
            let f = (e.frame.get() + k.get() as u64).checked_sub(lat as u64);
            if f.is_some_and(|f| f < self.kept[0].0) {
                self.clamped += 1;
            }
            let want = encode(&self.truth(f));
            assert_eq!(out[2][i], lat as f32, "{what}: arrival");
            assert_eq!(
                [out[0][i], out[1][i]],
                want,
                "{what}: frame {i} of block at {}",
                e.frame.get()
            );
        }
    }
}

/// Where `e`'s transport stands at its end: a host that counts frames starts
/// the next block there, and the history keeps no piece for it.
fn end_of(e: &Env) -> TransportPosition {
    let longer = Env {
        block_len: e.block_len + Samples::new(1),
        ..*e
    };
    longer.transport_at(Offset::of(e.block_len, longer.block_len))
}

/// Mostly a continuing roll, a `scene` every `every` blocks.
fn mostly_rolling(
    b: usize,
    every: usize,
    n: usize,
    prev: Option<&Env>,
) -> (TransportPosition, TransportChanges) {
    match prev {
        Some(p) if !b.is_multiple_of(every) => (end_of(p), TransportChanges::NONE),
        _ => scene(b / every, n, prev),
    }
}

fn rolling(beat: f64) -> TransportPosition {
    TransportPosition::new(true, Bpm::of(120.0), Beat::new(beat), None)
}

/// The block-`b` scene for a block of `n` frames after `prev` (the block
/// before, if any): a plain roll continuing it, a seek, a seek and tempo
/// change inside the block, a 40-frame loop starting at the block, the loop
/// continued from where it stood, a stop, and recording.
fn scene(b: usize, n: usize, prev: Option<&Env>) -> (TransportPosition, TransportChanges) {
    let at = |i: usize| Offset::of(Samples::new(i), Samples::new(n));
    let mut changes = TransportChanges::NONE;
    let t = match b % 7 {
        0 => prev.map_or(rolling(1.0), end_of),
        1 => rolling(16.0 + b as f64),
        2 => {
            if n > 1 {
                changes
                    .push(
                        at(n / 2),
                        TransportPosition::new(
                            true,
                            Bpm::of(90.0),
                            Beat::new(64.0 + b as f64),
                            None,
                        ),
                    )
                    .expect("inside");
            }
            rolling(8.0 + b as f64)
        }
        3 => TransportPosition::new(
            true,
            Bpm::of(120.0),
            Beat::new(4.0),
            Some(LoopRange::new(Beat::new(4.0), Beat::new(4.0 + 40.0 / FPB)).expect("a loop")),
        ),
        // The loop, picked up at the previous block's last frame.
        4 => prev.map_or(rolling(2.0), |p| {
            p.transport_at(Offset::of(
                p.block_len
                    .checked_sub(Samples::new(1))
                    .expect("a block has at least one frame"),
                p.block_len,
            ))
        }),
        5 => TransportPosition::new(false, Bpm::of(100.0), Beat::new(3.0), None),
        _ => rolling(32.0 + b as f64).with_recording(true),
    };
    (t, changes)
}

/// Ragged blocks behind a 100-frame path: every frame of every block reads
/// what the block that rendered its input said, two and more blocks back,
/// across every kind of transport discontinuity; the first 100 frames read
/// the first block's transport, unmoved. The probe's arrival is 100.
#[test]
fn a_node_behind_a_latent_path_reads_the_transport_its_input_was_rendered_at() {
    const LAT: usize = 100;
    let mut rig = Rig::new(LAT);
    let lens = [64usize, 17, 50, 1, 33, 64, 2, 40];
    for b in 0..60 {
        let n = lens[b % lens.len()];
        let (t, c) = scene(b, n, rig.played.last());
        let out = rig.block(n, t, c);
        rig.check(&out, LAT, &format!("block {b}"));
    }
}

/// A recompile that moves the arrival changes nothing in the history: it is
/// a fixed set of jumps, not sized from the plan. A host that counts frames
/// rolls on in one-frame blocks with a scene every 20, so the jumps inside
/// even the raised 300-frame arrival fit, and once the probe has 300 frames
/// behind it no frame reads the clamp, before or after either change.
#[test]
fn a_recompile_that_moves_the_arrival_keeps_the_history() {
    let mut rig = Rig::new(30);
    let blocks =
        |rig: &mut Rig, count: usize, n: &dyn Fn(usize) -> usize, lat: usize, what: &str| {
            for b in 0..count {
                let n = n(b);
                let (t, c) = mostly_rolling(b, 20, n, rig.played.last());
                let out = rig.block(n, t, c);
                rig.check(&out, lat, &format!("{what}, block {b}"));
            }
        };
    blocks(&mut rig, 400, &|_| 1, 30, "at 30");
    let before = rig.clamped;
    rig.set_latency(300);
    blocks(
        &mut rig,
        600,
        &|b| if b % 5 == 4 { 64 } else { 1 },
        300,
        "raised to 300",
    );
    rig.set_latency(10);
    blocks(&mut rig, 50, &|b| 1 + b % 3, 10, "lowered to 10");
    assert_eq!(
        rig.clamped, before,
        "no frame after the first read the clamp"
    );
}

/// A burst of seeks, one per one-frame block, past `JUMPS` inside the
/// arrival: the history keeps the newest 256, and the frames older than
/// those read the oldest kept, unmoved: exactly where the model says.
#[test]
fn a_burst_of_seeks_past_the_capacity_clamps_where_the_model_says() {
    const LAT: usize = 300;
    let mut rig = Rig::new(LAT);
    for b in 0..700 {
        // Seeks every frame (scene 1), then a counting host again.
        let (t, c) = if (200..500).contains(&b) {
            (rolling(16.0 + b as f64), TransportChanges::NONE)
        } else {
            mostly_rolling(b, 20, 1, rig.played.last())
        };
        let out = rig.block(1, t, c);
        rig.check(&out, LAT, &format!("block {b}"));
    }
    // Which frames clamp is the model's to say (checked frame by frame
    // above); here only that the burst did overflow: some 300 seeks inside a
    // 300-frame arrival leave about 90 one-frame blocks reading the clamp.
    assert!(rig.clamped > 50, "the burst overflowed the history");
}

/// A sample-rate change clears the history: the executor's clock is
/// rescaled to the new rate, and frames counted at the old one name nothing
/// there. The first blocks after it read the clamp (the first block at the
/// new rate, unmoved) until it has `reach` frames behind it; then every
/// frame reads its block again.
#[test]
fn a_sample_rate_change_starts_the_history_over() {
    const LAT: usize = 100;
    let mut rig = Rig::new(LAT);
    for b in 0..12 {
        let (t, c) = scene(b, 64, rig.played.last());
        let out = rig.block(64, t, c);
        rig.check(&out, LAT, &format!("48 kHz, block {b}"));
    }
    rig.reprepare(SampleRate::SR_96K);
    let before = rig.clamped;
    for b in 0..12 {
        // The first block at the new rate does not continue the old one.
        let prev = rig.played.last().filter(|_| b > 0);
        let (t, c) = scene(b, 64, prev);
        let out = rig.block(64, t, c);
        rig.check(&out, LAT, &format!("96 kHz, block {b}"));
    }
    assert!(
        rig.clamped - before >= LAT,
        "the first {LAT} frames after the change read the clamp ({} did)",
        rig.clamped - before
    );
}

/// Four latent paths (0, 37, 100 and 300 frames) into four probes: a graph
/// wide enough to spread over a pool.
fn wide(workers: usize) -> (Editor, Executor) {
    const LATS: [usize; 4] = [0, 37, 100, 300];
    let (mut ed, mut exec) = Editor::new(prepare(MAX));
    let mut outputs = Vec::new();
    for (i, &lat) in LATS.iter().enumerate() {
        let (latent, probe) = (NodeKey(10 + i as u64), NodeKey(20 + i as u64));
        ed.insert(latent, "latent", Unforkable(Latent(lat)));
        ed.insert(probe, "probe", Unforkable(Probe));
        ed.connect_edge(tutti_graph::EdgeId {
            sink: tutti_graph::PortSink::Port(InputPort::new(probe, 0)),
            from: Edge::Direct(PortSource::Node(OutputPort::new(latent, 0))),
        })
        .expect("the edge wires");
        outputs.extend((0..3).map(|port| PortSource::Node(OutputPort::new(probe, port))));
    }
    ed.set_outputs(outputs).expect("the sources exist");
    ed.commit().expect("commits");
    common::pooled_fresh(&mut exec, workers);
    (ed, exec)
}

/// **Parallel equals one-participant for the arrival transport.** The same wide graph
/// on a pool of 1 (one-participant), 2, 3 and 4 participants, through ragged blocks
/// and every scene (seeks, a change inside a block, a loop that wraps, a
/// stop, recording): every probe's output, the transport its input was
/// rendered at, is bit-identical to the one-participant render's. Every participant
/// reads the executor's one history, observed before the pool runs.
#[test]
fn the_parallel_render_reads_the_same_history_as_the_one_participant_one() {
    let lens = [64usize, 17, 50, 1, 33, 64, 2, 40];
    let render = |workers: usize| -> Vec<Vec<u32>> {
        let (mut ed, mut exec) = wide(workers);
        let mut out: Vec<Vec<u32>> = vec![Vec::new(); 12];
        let mut prev: Option<Env> = None;
        for b in 0..80 {
            let n = lens[b % lens.len()];
            let (t, c) = scene(b, n, prev.as_ref());
            let mut bufs: Vec<Vec<f32>> = vec![vec![0.0; n]; 12];
            {
                let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
                prev = Some(Env {
                    frame: exec.frame(),
                    sample_rate: prepare(MAX).sample_rate(),
                    block_len: Samples::new(n),
                    transport: t,
                    changes: c,
                    play: tutti_graph::PlayClock::ZERO,
                });
                exec.process_with_changes(n, &t, &c, &[], &mut refs);
            }
            ed.collect();
            assert_eq!(exec.is_parallel(), workers > 1, "{workers} workers");
            for (o, b) in out.iter_mut().zip(&bufs) {
                o.extend(b.iter().map(|x| x.to_bits()));
            }
        }
        out
    };
    common::agree_across_workers(2..=4, render, Vec::clone);
}
