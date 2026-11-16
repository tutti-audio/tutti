//! **A live plugin keeps in step with the device.** The reference CLAP
//! plugin in a real `plugin-server`, rendered by tutti-engine's `Engine` exactly
//! as a device drives it — one call per callback, the callbacks paced to real
//! time — at 480-, 1024- and 441-frame device quanta.
//!
//! A live plugin's pipeline chunk is the device's callback
//! (`Prepare::quantum`), not a fixed 64 frames. With 64, a chunk completed
//! mid-callback would be collected microseconds after it was submitted, and
//! read as silence; with the callback as the chunk, the server has a whole
//! device period to answer. Asserted here: every block is either the input
//! delayed by exactly the declared latency (137 + quantum) or silence, and the
//! graph declares that latency. A silent block is a chunk the node collected
//! unanswered (`PluginControls::missed_chunks`), and it is excused only when the
//! server then published that chunk before its ring slot was reused
//! (`late_chunks`), a late answer a loaded machine produces, or when it was never
//! seen published before its slot was reused (`lost_chunks`: the bridge skipped
//! it as more than a chunk behind, or the server refused it as overwritten
//! before it ran). A silent block whose chunk was neither, or that no collect
//! missed, fails.
//! A run in which more than a quarter of the judged blocks were late is rerun
//! (twice at most) or reported as too loaded to judge.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

use crate::clap_probe;
use clap_probe::{ProbeEnv, load_probe, render};
use tutti_fixtures::machine_lock;
use tutti_types::MaxBlock;

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tutti_buffers::InterleavedMut;
use tutti_engine::Engine;
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_plugin_ipc::RING_SLOTS;
use tutti_transport::Transport;
use tutti_types::ChannelLayout;
use tutti_types::{SampleRate, Samples, Tail};

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
    /// Blocks after warm-up.
    blocks: usize,
    /// Of those, entirely silent (block indices).
    silent: Vec<usize>,
    /// Of those, neither silent nor the input delayed by `declared`.
    wrong: Vec<usize>,
    /// The blocks whose collect missed its chunk (`missed_chunks` moved).
    misses: Vec<usize>,
    /// The blocks in which a missed chunk was counted late (`late_chunks`
    /// moved), once per chunk.
    lates: Vec<usize>,
    /// The blocks in which a missed chunk was counted lost (`lost_chunks`
    /// moved), once per chunk.
    losts: Vec<usize>,
}

/// Keeps idle CPUs awake for the length of a paced run (Linux only).
///
/// Measured on an idle 4-vCPU VM: a thread woken from an idle CPU is
/// sometimes 10 to 28 ms late (a bare `sleep(1 ms)` loop with no tutti code
/// oversleeps by more than 10 ms about once in 4000 calls), at any thread
/// priority, and the late thread is the host's bridge thread or the server's
/// about once in 1000 blocks. The plugin gets one period to answer, so such a
/// wake-up costs the block and its successors up to the next fresh one: the
/// source of the late answers `judge_silence` excuses. With spinners on the other
/// CPUs at the lowest priority (they yield to any waking thread) the late
/// wake-ups fell from 8 in 5249 blocks to 1 in 8399. A device's callback is
/// woken by an interrupt, not from an idle CPU, so this keeps the run closer
/// to one.
///
/// Safe where the CPU count is a quota: fewer than 4 CPUs spawn none, and
/// `cpus / 2` bounds the rest. Every caller holds [`exclusive`], so one
/// process spins at a time. A spinner whose `setpriority` fails exits rather
/// than spin at normal priority; other platforms spawn none.
struct KeepAwake {
    stop: Arc<AtomicBool>,
    spinners: Vec<JoinHandle<()>>,
}

/// Spinners for `cpus` CPUs.
fn spinner_count(cpus: usize) -> usize {
    if cfg!(target_os = "linux") && cpus >= 4 {
        cpus / 2
    } else {
        0
    }
}

impl KeepAwake {
    fn new() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        let spinners = (0..spinner_count(cpus))
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    // Nice 19 on the calling thread; a spinner that cannot
                    // get it would compete with the run.
                    #[cfg(target_os = "linux")]
                    // SAFETY: `setpriority` takes no pointers.
                    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) } != 0 {
                        return;
                    }
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        Self { stop, spinners }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.spinners.drain(..) {
            let _ = t.join();
        }
    }
}

/// Callbacks a [`run_live`] burst makes back to back.
const BURST: usize = 3;

/// How many blocks after a miss a loss may be counted: a chunk is counted lost
/// when its slot is reused, `RING_SLOTS` chunks on, noticed at a collect or
/// submit after that.
const LOST_WINDOW: usize = RING_SLOTS + 2;

/// The missed blocks whose chunk the plugin side accounts for: published
/// after all, or counted lost. A miss at block `b` is counted late in
/// `b` (the same callback's submit), `b + 1` (the next collect) or `b + 2`,
/// while the chunk's ring slot still shows it (`RING_SLOTS` chunks on); a
/// loss is counted within [`LOST_WINDOW`] blocks of it. Each count answers
/// one miss, the earliest.
fn excused(misses: &[usize], lates: &[usize], losts: &[usize]) -> Vec<usize> {
    let (mut lates, mut losts) = (lates.to_vec(), losts.to_vec());
    let mut excused = Vec::new();
    for &miss in misses {
        let take = |events: &mut Vec<usize>, window: usize| {
            events
                .iter()
                .position(|&e| (miss..=miss + window).contains(&e))
                .map(|at| events.remove(at))
                .is_some()
        };
        if take(&mut lates, RING_SLOTS) || take(&mut losts, LOST_WINDOW) {
            excused.push(miss);
        }
    }
    excused
}

/// `Ok` iff every silent block from `from` on, outside `neutral` (the burst
/// window: its blocks are not judged), was a missed chunk the server published
/// late.
fn judge_silence(run: &Run, from: usize, neutral: &Range<usize>) -> Result<(), String> {
    let late = excused(&run.misses, &run.lates, &run.losts);
    let mut unexplained = Vec::new();
    for &b in &run.silent {
        if b < from || neutral.contains(&b) {
            continue;
        }
        if !run.misses.contains(&b) {
            unexplained.push(format!("{b} (no collect missed: nothing was submitted)"));
        } else if !late.contains(&b) {
            unexplained.push(format!(
                "{b} (its chunk was neither published late nor counted lost)"
            ));
        }
    }
    if unexplained.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "silent blocks outside a late answer or a lost count: {unexplained:?} \
             (misses {:?}, late counts at {:?}, lost counts at {:?})",
            run.misses, run.lates, run.losts
        ))
    }
}

/// Render `blocks` callbacks of `quantum` frames at `rate`, paced to real
/// time, through an engine whose graph is the ramp into the probe. With
/// `burst_at`, the callbacks from that block (counted after warm-up) on run
/// [`BURST`] in a row with no pause, as a device does when it catches up.
fn run_live(rate: f64, quantum: usize, blocks: usize, burst_at: Option<usize>) -> Run {
    let probe = load_probe(rate);
    let _awake = KeepAwake::new();
    let inputs = probe.client.inputs();
    let prepare = Prepare::new(SampleRate::of(rate), MaxBlock::of(Samples(quantum)))
        .with_quantum(MaxBlock::of(Samples(quantum)));
    let (mut ed, exec) = Editor::new(prepare);
    let (src, key) = (NodeKey(1), NodeKey(2));
    ed.insert(src, "ramp", Unforkable(RampSource));
    let controls = ed
        .insert(
            key,
            "plugin",
            probe.client.bind(
                tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0))
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
            vec![Edge::Direct(Source::Node(OutPort { node: src, port: 0 }))],
        );
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    ed.commit().expect("the graph compiles");
    let declared = ed.spec().topology.nodes[&key].latency;

    let transport = Transport::new(SampleRate::of(rate));
    let engine = Engine::new(&transport, &mut ed, exec).expect("the engine builds");
    let period = Duration::from_secs_f64(quantum as f64 / rate);
    let warm_up = 8 + declared.get().div_ceil(quantum);
    let mut buf = vec![0.0f32; quantum];
    let (mut silent, mut wrong) = (Vec::new(), Vec::new());
    let (mut misses, mut lates, mut losts) = (Vec::new(), Vec::new(), Vec::new());
    let mut next = Instant::now();
    // `LOST_WINDOW` more callbacks, unjudged, so a chunk missed at the end is
    // counted late or lost before the counts are read.
    for b in 0..warm_up + blocks + LOST_WINDOW {
        let (missed, late, lost) = (
            controls.missed_chunks(),
            controls.late_chunks(),
            controls.lost_chunks(),
        );
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO).unwrap());
        ed.collect();
        if b >= warm_up {
            let at = b - warm_up;
            let first = (b * quantum) as u64;
            if controls.missed_chunks() > missed {
                misses.push(at);
            }
            for _ in late..controls.late_chunks() {
                lates.push(at);
            }
            for _ in lost..controls.lost_chunks() {
                losts.push(at);
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
        // A device's callbacks stay a period apart: one that oversleeps (a
        // loaded machine) is not followed by a burst of catch-up calls, which
        // would hand the server no period to answer and so read as silence.
        let now = Instant::now();
        next += period;
        if next <= now {
            next = now + period;
        }
        std::thread::sleep(next - now);
    }
    drop(probe.handle);
    Run {
        declared,
        blocks,
        silent,
        wrong,
        misses,
        lates,
        losts,
    }
}

#[test]
fn a_miss_is_accounted_for_only_by_a_late_or_lost_count_in_its_window() {
    assert!(excused(&[], &[], &[]).is_empty());
    assert_eq!(
        excused(&[10], &[10], &[]),
        [10],
        "counted in its own callback"
    );
    assert_eq!(excused(&[10], &[11], &[]), [10]);
    assert_eq!(
        excused(&[10], &[12], &[]),
        [10],
        "while its slot still shows it"
    );
    assert!(excused(&[10], &[13], &[]).is_empty(), "past the ring");
    assert!(excused(&[10], &[9], &[]).is_empty(), "before the miss");
    assert!(excused(&[10], &[], &[]).is_empty(), "never published");
    assert_eq!(
        excused(&[10, 11], &[12], &[]),
        [10],
        "one late count answers one miss, the earliest"
    );
    assert_eq!(
        excused(&[10], &[], &[13]),
        [10],
        "a loss, noticed a stall behind"
    );
    assert!(
        excused(&[10], &[], &[15]).is_empty(),
        "a loss past its window"
    );
    assert_eq!(
        excused(&[10, 11], &[], &[11]),
        [10],
        "one lost count answers one miss"
    );
}

#[test]
fn a_silent_block_is_judged_by_what_the_collect_and_the_server_did() {
    let run = |silent: &[usize], misses: &[usize], lates: &[usize], losts: &[usize]| Run {
        declared: Samples(0),
        blocks: 200,
        silent: silent.to_vec(),
        wrong: Vec::new(),
        misses: misses.to_vec(),
        lates: lates.to_vec(),
        losts: losts.to_vec(),
    };
    assert!(judge_silence(&run(&[], &[], &[], &[]), 0, &(0..0)).is_ok());
    assert!(judge_silence(&run(&[20], &[20], &[21], &[]), 0, &(0..0)).is_ok());
    assert!(
        judge_silence(&run(&[20], &[20], &[], &[22]), 0, &(0..0)).is_ok(),
        "a chunk counted lost"
    );
    let never = judge_silence(&run(&[20], &[20], &[], &[]), 0, &(0..0)).unwrap_err();
    assert!(
        never.contains("neither published late nor counted lost"),
        "{never}"
    );
    let none = judge_silence(&run(&[20], &[], &[], &[]), 0, &(0..0)).unwrap_err();
    assert!(none.contains("nothing was submitted"), "{none}");
    assert!(
        judge_silence(&run(&[61, 62], &[], &[], &[]), 0, &(60..65)).is_ok(),
        "the neutral window is not judged"
    );
    assert!(
        judge_silence(&run(&[3], &[], &[], &[]), 10, &(0..0)).is_ok(),
        "before `from` is not judged"
    );
}

/// Runs of `run` before a machine is called too loaded to judge.
const JUDGEABLE_RUNS: usize = 3;

/// The first run of `run` that can be judged: no block wrong, no silent block
/// outside a late answer or a lost count (either fails at once, in any run, whatever
/// the load), and no more than a quarter of the blocks judged (`from` on,
/// outside `neutral`) answered late or lost. More than that and the run asserts
/// little about the pipeline, so it is run again on a fresh server, up to
/// [`JUDGEABLE_RUNS`] times, and then reported.
fn judgeable_run(
    what: &str,
    mut run: impl FnMut() -> Run,
    from: usize,
    neutral: Range<usize>,
) -> Run {
    let mut seen = Vec::new();
    for attempt in 1..=JUDGEABLE_RUNS {
        let run = run();
        assert!(
            run.wrong.is_empty(),
            "{what}: blocks {:?} are neither silent nor the input delayed by {:?}",
            run.wrong,
            run.declared
        );
        let judged = (from..run.blocks).filter(|b| !neutral.contains(b)).count();
        let late = run
            .lates
            .iter()
            .chain(&run.losts)
            .filter(|&&b| b >= from && !neutral.contains(&b))
            .count();
        eprintln!(
            "{what}: run {attempt}: {} chunks missed, {late} late or lost ({} lost), of \
             {judged} judged blocks, {} silent",
            run.misses.len(),
            run.losts.len(),
            run.silent.len()
        );
        if let Err(why) = judge_silence(&run, from, &neutral) {
            panic!("{what}: {why}");
        }
        if late * 4 <= judged {
            return run;
        }
        seen.push(format!("{late} of {judged}"));
    }
    panic!("{what}: machine too loaded to judge: late or lost chunks per run: {seen:?}");
}

fn assert_live(rate: f64, quantum: usize) {
    let what = format!("{quantum}-frame callbacks");
    let run = judgeable_run(&what, || run_live(rate, quantum, 200, None), 0, 0..0);
    assert_eq!(
        run.declared,
        Samples(PROBE_LATENCY + quantum),
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
    let run = judgeable_run(
        "a burst of callbacks",
        || run_live(48_000.0, QUANTUM, BLOCKS, Some(BURST_AT)),
        SETTLE,
        window,
    );
    eprintln!("burst at {BURST_AT}: silent blocks {:?}", run.silent);
}
