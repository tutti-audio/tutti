//! **A live plugin keeps in step with the device.** The reference CLAP
//! plugin in a real `plugin-server`, rendered by tutti-engine's `Engine` exactly
//! as a device drives it — one call per callback, each once the chunk before it
//! is settled — at 480-, 1024- and 441-frame device quanta.
//!
//! A live plugin's pipeline chunk is the device's callback
//! (`Prepare::quantum`), not a fixed 64 frames. With 64, a chunk completed
//! mid-callback would be collected microseconds after it was submitted, and
//! read as silence; with the callback as the chunk, the server has a whole
//! device period to answer. Asserted here: every block is either the input
//! delayed by exactly the declared latency (137 + quantum) or silence, and the
//! graph declares that latency. A silent block is a chunk the node collected
//! unanswered (`ChunkOutcomes::missed`), and a missed chunk's fate is one of
//! four: published after all (`late`, a late answer a loaded
//! machine produces), refused by the server, which did not publish it
//! (`refused`: its input slot was recycled before it ran), skipped by
//! the bridge as more than a chunk behind (`skipped`), or unanswered
//! (`unanswered`). A silent block is excused only by the first three,
//! counted within a few blocks of the miss; a silent block whose chunk was none
//! of these, or that no collect missed, fails; so does a run with an
//! unanswered chunk: a server that drops a chunk silently.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

use crate::clap_probe;
use clap_probe::{ProbeEnv, load_probe, render};
use tutti_buffers::MaxBlock;
use tutti_fixtures::machine_lock;

use std::ops::Range;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_engine::Engine;
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_graph::{Edge, InPort, OutPort, PortSource};
use tutti_plugin_ipc::RING_SLOTS;
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples, Tail};

/// What the probe declares and delays by (`REPORTED_LATENCY_SAMPLES`).
const PROBE_LATENCY: usize = 137;

/// Input frame `t`: distinct, never zero, exact in `f32`.
fn ramp(t: u64) -> f32 {
    ((t % 997) + 1) as f32 / 1024.0
}

/// The ramp as a graph source, read off each block's `Env`.
struct RampSource;

impl Node for RampSource {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let first = cx.env.frame.get();
        for (i, s) in io.output(0).iter_mut().enumerate() {
            *s = ramp(first + i as u64);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// What a live run saw.
struct Run {
    declared: Samples,
    /// The blocks after warm-up that were entirely silent (indices).
    silent: Vec<usize>,
    /// Of those, neither silent nor the input delayed by `declared`.
    wrong: Vec<usize>,
    /// The blocks whose collect missed its chunk (`missed` moved).
    misses: Vec<usize>,
    /// The blocks in which a missed chunk was counted late (`late`
    /// moved), once per chunk.
    lates: Vec<usize>,
    /// The blocks in which the server refused a chunk (`refused`
    /// moved), once per chunk.
    refuses: Vec<usize>,
    /// The blocks in which the bridge skipped a chunk as stale
    /// (`skipped` moved), once per chunk.
    skips: Vec<usize>,
    /// Missed chunks no counter explains at the end of the run
    /// (`unanswered`).
    unanswered: u64,
}

/// Callbacks a [`run_live`] burst makes back to back.
const BURST: usize = 3;

/// The unjudged callbacks after a run's last block.
const TRAILING: usize = 16;

/// How many blocks after a miss a late answer, a refusal or a skip may be
/// counted: the bridge thread learns each, and it can be a stall behind the
/// audio thread. A late answer is seen in the shared ring within `RING_SLOTS`
/// chunks, or later in the server's reply once the slot has been reused, so it
/// has the window of the others. The strong check is that no missed chunk is
/// unanswered.
const SKIP_WINDOW: usize = RING_SLOTS + 2;

/// The missed blocks whose chunk the plugin side accounts for: published
/// after all, refused by the server, or skipped by the bridge. Each is counted
/// within [`SKIP_WINDOW`] blocks of the miss, and answers one miss. The three
/// kinds share the window, so they are pooled and each miss, earliest first,
/// takes the earliest count in its window: that matches as many misses as any
/// assignment does, whatever the kind of a count.
fn excused(misses: &[usize], lates: &[usize], refuses: &[usize], skips: &[usize]) -> Vec<usize> {
    let mut counts: Vec<usize> = [lates, refuses, skips].concat();
    counts.sort_unstable();
    let mut excused = Vec::new();
    for &miss in misses {
        if let Some(at) = counts
            .iter()
            .position(|&e| (miss..=miss + SKIP_WINDOW).contains(&e))
        {
            counts.remove(at);
            excused.push(miss);
        }
    }
    excused
}

/// `Ok` iff every silent block from `from` on, outside `neutral` (the burst
/// window: its blocks are not judged), was a missed chunk the server published
/// late.
fn judge_silence(run: &Run, from: usize, neutral: &Range<usize>) -> Result<(), String> {
    let late = excused(&run.misses, &run.lates, &run.refuses, &run.skips);
    let mut unexplained = Vec::new();
    for &b in &run.silent {
        if b < from || neutral.contains(&b) {
            continue;
        }
        if !run.misses.contains(&b) {
            unexplained.push(format!("{b} (no collect missed: nothing was submitted)"));
        } else if !late.contains(&b) {
            unexplained.push(format!(
                "{b} (its chunk was never published, nor counted refused or skipped)"
            ));
        }
    }
    if unexplained.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "silent blocks outside a late answer, a refusal or a skip: {unexplained:?} \
             (misses {:?}, late counts at {:?}, refusals at {:?}, skips at {:?})",
            run.misses, run.lates, run.refuses, run.skips
        ))
    }
}

/// Waits until the chunk the node submitted last is settled: answered, or
/// counted refused or skipped since `refused` and `skipped` were read.
fn settled(controls: &tutti_plugin_host::PluginControls, refused: u64, skipped: u64) {
    clap_probe::await_answer_or(
        controls,
        "the chunk in flight to be answered, refused or skipped",
        || {
            controls.chunk_outcomes().refused > refused
                || controls.chunk_outcomes().skipped > skipped
        },
    );
}

/// Render `blocks` callbacks of `quantum` frames at `rate`, each once the
/// chunk before it is settled, through an engine whose graph is the ramp into the probe. With
/// `burst_at`, the callbacks from that block (counted after warm-up) on run
/// [`BURST`] in a row with no pause, as a device does when it catches up.
fn run_live(rate: f64, quantum: usize, blocks: usize, burst_at: Option<usize>) -> Run {
    let probe = load_probe(rate);
    let inputs = probe.client.inputs();
    let prepare = Prepare::new(SampleRate::of(rate), MaxBlock::of(Samples::new(quantum)))
        .with_quantum(MaxBlock::of(Samples::new(quantum)));
    let (mut ed, exec) = Editor::new(prepare);
    let (src, key) = (NodeKey(1), NodeKey(2));
    ed.insert(src, "ramp", Unforkable(RampSource));
    let controls = ed
        .insert(
            key,
            "plugin",
            probe.client.bind(
                tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0))
                    .expect("a tempo"),
            ),
        )
        .controls;
    for port in 0..inputs {
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: key,
                port: port as u16,
            },
            vec![Edge::Direct(PortSource::Node(OutPort {
                node: src,
                port: 0,
            }))],
        );
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutPort { node: key, port: 0 })]);
    ed.commit().expect("the graph compiles");
    let declared = ed.spec().topology.nodes[&key].latency;

    let transport = Transport::new(SampleRate::of(rate));
    let engine = Engine::new(&transport, &mut ed, exec).expect("the engine builds");
    let warm_up = 8 + declared.get().div_ceil(quantum);
    let mut buf = vec![0.0f32; quantum];
    let (mut silent, mut wrong) = (Vec::new(), Vec::new());
    let (mut misses, mut lates) = (Vec::new(), Vec::new());
    let (mut refuses, mut skips) = (Vec::new(), Vec::new());
    let mut skipped = 0;
    // More callbacks, unjudged, so a chunk missed at the end is resolved before
    // the counts are read: the batcher gives up on a fate it has not learned
    // `TRAILING` chunks on.
    for b in 0..warm_up + blocks + TRAILING {
        // The bridge thread counts a skip when it gets to the chunk, between
        // callbacks: what it counted since the last callback began is the
        // last block's.
        let now_skipped = controls.chunk_outcomes().skipped;
        if b > warm_up {
            skips.extend((skipped..now_skipped).map(|_| b - 1 - warm_up));
        }
        skipped = now_skipped;
        let (missed, late, refused) = (
            controls.chunk_outcomes().missed,
            controls.chunk_outcomes().late,
            controls.chunk_outcomes().refused,
        );
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO).unwrap());
        ed.collect();
        if b >= warm_up {
            let at = b - warm_up;
            let first = (b * quantum) as u64;
            if controls.chunk_outcomes().missed > missed {
                misses.push(at);
            }
            for _ in late..controls.chunk_outcomes().late {
                lates.push(at);
            }
            for _ in refused..controls.chunk_outcomes().refused {
                refuses.push(at);
            }
            let judged = at < blocks;
            if judged && buf.iter().all(|&s| s == 0.0) {
                silent.push(at);
            } else if judged
                && buf
                    .iter()
                    .enumerate()
                    .any(|(i, &s)| s != ramp(first + i as u64 - declared.get() as u64))
            {
                wrong.push(b);
            }
        }
        if burst_at.is_some_and(|at| (warm_up + at..warm_up + at + BURST - 1).contains(&b)) {
            continue;
        }
        // The next callback comes once the chunk this one submitted is settled
        // (answered, or counted refused or skipped): the server has the whole
        // period to answer, and a callback is never a catch-up that would hand
        // it none and read as silence.
        settled(&controls, refused, now_skipped);
    }
    skips.extend((skipped..controls.chunk_outcomes().skipped).map(|_| blocks + TRAILING - 1));
    let unanswered = controls.chunk_outcomes().unanswered;
    drop(probe.handle);
    Run {
        declared,
        silent,
        wrong,
        misses,
        lates,
        refuses,
        skips,
        unanswered,
    }
}

#[test]
fn a_miss_is_accounted_for_only_by_a_late_refused_or_skipped_count_in_its_window() {
    assert!(excused(&[], &[], &[], &[]).is_empty());
    assert_eq!(
        excused(&[10], &[10], &[], &[]),
        [10],
        "counted in its own callback"
    );
    assert_eq!(excused(&[10], &[11], &[], &[]), [10]);
    assert_eq!(
        excused(&[10], &[12], &[], &[]),
        [10],
        "while its slot still shows it"
    );
    assert_eq!(
        excused(&[10], &[13], &[], &[]),
        [10],
        "a reply that says published, after the slot was reused"
    );
    assert!(
        excused(&[10], &[15], &[], &[]).is_empty(),
        "past the window"
    );
    assert!(excused(&[10], &[9], &[], &[]).is_empty(), "before the miss");
    assert!(excused(&[10], &[], &[], &[]).is_empty(), "never published");
    assert_eq!(
        excused(&[10, 11], &[12], &[], &[]),
        [10],
        "one late count answers one miss, the earliest"
    );
    assert_eq!(
        excused(&[10], &[], &[13], &[]),
        [10],
        "a refusal, a stall behind"
    );
    assert_eq!(
        excused(&[10], &[], &[], &[13]),
        [10],
        "a skip, a stall behind"
    );
    assert!(
        excused(&[10], &[], &[15], &[]).is_empty(),
        "a refusal past its window"
    );
    assert!(
        excused(&[10], &[], &[], &[15]).is_empty(),
        "a skip past its window"
    );
    assert_eq!(
        excused(&[10, 11], &[], &[11], &[]),
        [10],
        "one refusal answers one miss"
    );
    assert_eq!(
        excused(&[10, 11], &[], &[], &[11]),
        [10],
        "one skip answers one miss"
    );
    assert_eq!(
        excused(&[61, 62, 63, 65], &[64, 66], &[64], &[62]),
        [61, 62, 63, 65],
        "a late count of one kind never starves a miss another kind's count answers"
    );
}

#[test]
fn a_silent_block_is_judged_by_what_the_collect_and_the_server_did() {
    let run = |silent: &[usize],
               misses: &[usize],
               lates: &[usize],
               refuses: &[usize],
               skips: &[usize]| Run {
        declared: Samples::new(0),
        silent: silent.to_vec(),
        wrong: Vec::new(),
        misses: misses.to_vec(),
        lates: lates.to_vec(),
        refuses: refuses.to_vec(),
        skips: skips.to_vec(),
        unanswered: 0,
    };
    assert!(judge_silence(&run(&[], &[], &[], &[], &[]), 0, &(0..0)).is_ok());
    assert!(judge_silence(&run(&[20], &[20], &[21], &[], &[]), 0, &(0..0)).is_ok());
    assert!(
        judge_silence(&run(&[20], &[20], &[], &[22], &[]), 0, &(0..0)).is_ok(),
        "a chunk the server refused"
    );
    assert!(
        judge_silence(&run(&[20], &[20], &[], &[], &[22]), 0, &(0..0)).is_ok(),
        "a chunk the bridge skipped"
    );
    let never = judge_silence(&run(&[20], &[20], &[], &[], &[]), 0, &(0..0)).unwrap_err();
    assert!(never.contains("never published"), "{never}");
    let none = judge_silence(&run(&[20], &[], &[], &[], &[]), 0, &(0..0)).unwrap_err();
    assert!(none.contains("nothing was submitted"), "{none}");
    assert!(
        judge_silence(&run(&[61, 62], &[], &[], &[], &[]), 0, &(60..65)).is_ok(),
        "the neutral window is not judged"
    );
    assert!(
        judge_silence(&run(&[3], &[], &[], &[], &[]), 10, &(0..0)).is_ok(),
        "before `from` is not judged"
    );
}

/// Runs `run` and judges it: no block wrong, no silent block outside a late
/// answer, a refusal or a skip (from `from` on, outside `neutral`), and no
/// missed chunk the server never accounted for.
fn judged_run(what: &str, run: impl FnOnce() -> Run, from: usize, neutral: Range<usize>) -> Run {
    let run = run();
    assert!(
        run.wrong.is_empty(),
        "{what}: blocks {:?} are neither silent nor the input delayed by {:?}",
        run.wrong,
        run.declared
    );
    eprintln!(
        "{what}: {} chunks missed ({} late, {} refused, {} skipped), {} silent",
        run.misses.len(),
        run.lates.len(),
        run.refuses.len(),
        run.skips.len(),
        run.silent.len()
    );
    assert_eq!(
        run.unanswered, 0,
        "{what}: a missed chunk the server neither answered, refused nor skipped"
    );
    if let Err(why) = judge_silence(&run, from, &neutral) {
        panic!("{what}: {why}");
    }
    run
}

fn assert_live(rate: f64, quantum: usize) {
    let what = format!("{quantum}-frame callbacks");
    let run = judged_run(&what, || run_live(rate, quantum, 200, None), 0, 0..0);
    assert_eq!(
        run.declared,
        Samples::new(PROBE_LATENCY + quantum),
        "a {quantum}-frame device: the plugin's latency plus one callback"
    );
}

/// A device hears its plugin at each callback size: 480 frames at 48 kHz (not
/// a multiple of 64, so a 64-frame pipeline would collect most chunks
/// mid-callback; ignoring `Prepare::quantum` is invisible here, since the
/// graph's `MaxBlock` is the same 480, and the batcher's unit test pins it),
/// 1024 at 48 kHz (sixteen 64-frame passes per callback, of which the reversed
/// pipeline could fill only the first) and 441 at 44.1 kHz (10 ms: odd, so no
/// power-of-two chunk could line up with it).
#[test]
fn a_device_hears_its_plugin_at_every_callback_size() {
    for (rate, quantum) in [(48_000.0, 480), (48_000.0, 1024), (44_100.0, 441)] {
        let _lock = machine_lock();
        let _env = ProbeEnv::new().render_mode(render::LATENCY);
        assert_live(rate, quantum);
    }
}

/// **A burst of catch-up callbacks costs blocks, never correctness.** The
/// paced runs above no longer exercise back-to-back calls, which hand the
/// server no period to answer in. The slab ring is `RING_SLOTS` (2) deep and a
/// block more than `MAX_BEHIND` (`RING_SLOTS - 1`, 1) behind the newest is
/// dropped by design, so a burst of [`BURST`] callbacks may lose blocks as
/// silence, and the loss is bounded by the burst and its ring: the
/// [`BURST`] callbacks plus `RING_SLOTS` more while the pipeline refills.
/// Asserted: no block is ever neither silent nor the delayed input (a dropped
/// block must not surface stale or torn data), and outside that window the
/// stream is as sound as a paced run (a silent block is a late answer),
/// counted from `SETTLE` blocks after the run's warm-up: a loaded machine can
/// still be starting the server's pipeline then, which says nothing about the
/// burst. How many blocks the burst itself drops depends on how fast the server
/// answers, so that number is not asserted.
///
/// The mechanism of the drop is pinned by `dispatch`'s unit tests; this pins the
/// outcome a device sees.
#[test]
fn a_burst_of_callbacks_loses_blocks_but_never_corrupts_them() {
    const QUANTUM: usize = 480;
    const BLOCKS: usize = 200;
    const BURST_AT: usize = 60;
    /// Blocks after the run's warm-up that may still be start-up silence.
    const SETTLE: usize = 10;
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);

    let window = BURST_AT..BURST_AT + BURST + RING_SLOTS;
    let run = judged_run(
        "a burst of callbacks",
        || run_live(48_000.0, QUANTUM, BLOCKS, Some(BURST_AT)),
        SETTLE,
        window,
    );
    eprintln!("burst at {BURST_AT}: silent blocks {:?}", run.silent);
}
