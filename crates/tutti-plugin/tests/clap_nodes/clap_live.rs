//! **A live plugin keeps in step with the device.** The reference CLAP
//! plugin in a real `plugin-server`, rendered by tutti-engine's `Engine` exactly
//! as a device drives it — one call per callback, the callbacks paced to real
//! time — at 480-, 1024- and 441-frame device quanta.
//!
//! A live plugin's pipeline chunk is the device's callback
//! (`Prepare::quantum`), not a fixed 64 frames. With 64, a chunk completed
//! mid-callback would be collected microseconds after it was submitted, and
//! read as silence; with the callback as the chunk, the server has a whole
//! device period to answer. Asserted here: a block after warm-up is silent
//! only where the machine itself was measured stalled (`Canaries`, and the
//! pacing sleep's own overshoot), from the stall's start to the pipeline's
//! refill after its end; every block is either the input delayed by exactly the
//! declared latency (137 + quantum) or silence, and the graph declares that
//! latency. On a machine that did not stall, one silent block fails, and a run
//! whose stalls excuse more than a quarter of its judged blocks is rerun (twice
//! at most) or reported as too loaded to judge. A stall local to the server
//! process that no canary sees is an accepted limit: that block fails.
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
    /// The blocks a measured stall explains ([`excused_blocks`]).
    excused: Vec<usize>,
    /// How many stalls were measured ([`late`] or later) over the run.
    stalls: usize,
}

/// Keeps idle CPUs awake for the length of a paced run (Linux only).
///
/// Measured on an idle 4-vCPU VM: a thread woken from an idle CPU is
/// sometimes 10 to 28 ms late (a bare `sleep(1 ms)` loop with no tutti code
/// oversleeps by more than 10 ms about once in 4000 calls), at any thread
/// priority, and the late thread is the host's bridge thread or the server's
/// about once in 1000 blocks. The plugin gets one period to answer, so such a
/// wake-up costs the block and its successors up to the next fresh one: the
/// source of the stalls the [`Canaries`] measure. With spinners on the other
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

/// A wake-up this much past its sleep is a machine stalled: a thread woken from
/// an idle or contended CPU runs late, and the bridge thread and the server
/// are woken the same way. A block's reply is due one period after its
/// callback (10 ms at 480 frames, 48 kHz), so only a stall of this order can
/// cost one: 5 ms, and no more than half a period.
fn late(period: Duration) -> Duration {
    Duration::from_millis(5).min(period / 2)
}

/// Threads sleeping 1 ms in a loop, as the bridge thread and the server sleep,
/// recording every wake-up [`late`] or later as the interval the thread was
/// not running. The server's and the bridge's stalls are invisible to the test
/// thread, so the machine's own scheduling is measured beside the run, from
/// other threads, and a block is excused only where it shows a stall.
struct Canaries {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Vec<(Instant, Instant)>>>,
}

impl Canaries {
    fn new(late: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut stalls = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        let slept = Instant::now();
                        std::thread::sleep(Duration::from_millis(1));
                        let woke = Instant::now();
                        if woke - slept > Duration::from_millis(1) + late {
                            stalls.push((slept + Duration::from_millis(1), woke));
                        }
                    }
                    stalls
                })
            })
            .collect();
        Self { stop, threads }
    }

    /// Stops the threads; the stalls they saw, as `(start, end)`.
    fn finish(self) -> Vec<(Instant, Instant)> {
        self.stop.store(true, Ordering::Relaxed);
        self.threads
            .into_iter()
            .flat_map(|t| t.join().unwrap_or_default())
            .collect()
    }
}

/// The post-warm-up blocks a stall explains. `starts` is when each callback
/// began (warm-up included, `warm_up` of them first). A stall over `[from, to]`
/// explains the block before the one it began in (the server's stall began a
/// little before a canary saw it) through the block it ended in plus the
/// ring's `RING_SLOTS` while the pipeline refills.
fn excused_blocks(
    starts: &[Instant],
    warm_up: usize,
    blocks: usize,
    stalls: &[(Instant, Instant)],
) -> Vec<usize> {
    let block_at = |t: Instant| (starts.partition_point(|&s| s <= t) as i64 - 1) - warm_up as i64;
    let mut excused = Vec::new();
    for &(from, to) in stalls {
        let (first, last) = (block_at(from) - 1, block_at(to) + RING_SLOTS as i64);
        if last < 0 {
            continue;
        }
        let last = last.min(blocks as i64 - 1);
        excused.extend((first.max(0)..=last).map(|b| b as usize));
    }
    excused.sort_unstable();
    excused.dedup();
    excused
}

/// `Ok` iff every silent block is `excused` by a measured stall, or in
/// `neutral` (the burst window: its blocks are not judged).
fn judge_silence(silent: &[usize], excused: &[usize], neutral: Range<usize>) -> Result<(), String> {
    let unexplained: Vec<usize> = silent
        .iter()
        .copied()
        .filter(|b| !excused.contains(b) && !neutral.contains(b))
        .collect();
    if unexplained.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "silent blocks {unexplained:?} are outside every measured stall \
             (silent {silent:?}, explained by a stall {excused:?})"
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
    let _controls = ed.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
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
    let late = late(period);
    let canaries = Canaries::new(late);
    let warm_up = 8 + declared.get().div_ceil(quantum);
    let mut buf = vec![0.0f32; quantum];
    let (mut silent, mut wrong) = (Vec::new(), Vec::new());
    let mut starts = Vec::with_capacity(warm_up + blocks);
    let mut stalls = Vec::new();
    let mut next = Instant::now();
    for b in 0..warm_up + blocks {
        starts.push(Instant::now());
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO).unwrap());
        ed.collect();
        if b >= warm_up {
            let first = (b * quantum) as u64;
            if buf.iter().all(|&s| s == 0.0) {
                silent.push(b - warm_up);
            } else if buf
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
        let woke = Instant::now();
        if woke - next > late {
            stalls.push((next, woke));
        }
    }
    drop(probe.handle);
    stalls.extend(canaries.finish());
    let excused = excused_blocks(&starts, warm_up, blocks, &stalls);
    Run {
        declared,
        blocks,
        silent,
        wrong,
        excused,
        stalls: stalls.len(),
    }
}

#[test]
fn a_silent_block_is_excused_only_inside_a_measured_stall() {
    assert!(judge_silence(&[], &[], 0..0).is_ok());
    assert!(judge_silence(&[20, 21, 22], &[19, 20, 21, 22, 23], 0..0).is_ok());
    let err = judge_silence(&[20, 31], &[19, 20, 21], 0..0).unwrap_err();
    assert!(err.contains("[31]"), "names the unexplained block: {err}");
    assert!(
        judge_silence(&[9], &[], 0..0).is_err(),
        "on a machine that did not stall, one silent block is a fault"
    );
    assert!(
        judge_silence(&[61, 62, 63], &[], 60..65).is_ok(),
        "the neutral window is not judged"
    );
    assert!(judge_silence(&[65], &[], 60..65).is_err(), "past it is");
}

#[test]
fn a_stall_excuses_its_blocks_and_the_ring_refill_after_it() {
    let base = Instant::now();
    let ms = |n: u64| base + Duration::from_millis(n);
    // 30 callbacks 10 ms apart, 3 of them warm-up.
    let starts: Vec<Instant> = (0..30).map(|b| ms(b * 10)).collect();
    // A stall over callbacks 13 (post-warm-up block 10): one block before it
    // through the block it ends in, plus the ring.
    let got = excused_blocks(&starts, 3, 27, &[(ms(132), ms(138))]);
    let want: Vec<usize> = (9..=10 + RING_SLOTS).collect();
    assert_eq!(got, want);
    assert!(
        excused_blocks(&starts, 3, 27, &[(ms(1), ms(8))]).is_empty(),
        "a stall that ended in warm-up excuses nothing"
    );
    let end = excused_blocks(&starts, 3, 27, &[(ms(285), ms(299))]);
    assert_eq!(end.last(), Some(&26), "clamped to the run's last block");
    assert_eq!(
        excused_blocks(&starts, 3, 27, &[(ms(132), ms(138)), (ms(134), ms(139))]),
        want,
        "overlapping stalls count once"
    );
}

/// Runs a run whose stalls may excuse this many judged blocks, at most.
const MAX_EXCUSED: (usize, usize) = (1, 4);

/// Runs of `run` before a machine is called too loaded to judge.
const JUDGEABLE_RUNS: usize = 3;

/// The first run of `run` that can be judged: no block wrong, and its stalls
/// excuse at most a quarter of the blocks judged (`from` on, outside
/// `neutral`). More than that and a stall explains nearly every block, so the
/// run asserts almost nothing: it is run again on a fresh server, up to
/// [`JUDGEABLE_RUNS`] times, and then reported. A run under the cap is
/// returned whatever its silence is, and a wrong block fails at once.
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
        let excused = run
            .excused
            .iter()
            .filter(|&&b| b >= from && !neutral.contains(&b))
            .count();
        eprintln!(
            "{what}: run {attempt}: {} of {} judged blocks excused by {} measured stalls, {} silent",
            excused,
            judged,
            run.stalls,
            run.silent.len()
        );
        if excused * MAX_EXCUSED.1 <= judged * MAX_EXCUSED.0 {
            return run;
        }
        seen.push(format!("{excused} of {judged}"));
    }
    panic!("{what}: machine too loaded to judge: blocks excused per run: {seen:?}");
}

fn assert_live(rate: f64, quantum: usize) {
    let what = format!("{quantum}-frame callbacks");
    let run = judgeable_run(&what, || run_live(rate, quantum, 200, None), 0, 0..0);
    if let Err(why) = judge_silence(&run.silent, &run.excused, 0..0) {
        panic!(
            "{what}: {why} of {} blocks, {} stalls measured: the server was not given the \
             device period to answer",
            run.blocks, run.stalls
        );
    }
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
/// stream is as sound as a paced run (silent only inside a measured stall),
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
        window.clone(),
    );
    eprintln!("burst at {BURST_AT}: silent blocks {:?}", run.silent);
    let silent: Vec<usize> = run
        .silent
        .iter()
        .copied()
        .filter(|&b| b >= SETTLE)
        .collect();
    if let Err(why) = judge_silence(&silent, &run.excused, window.clone()) {
        panic!(
            "outside the burst window {window:?} from {SETTLE}: {why}; {} stalls measured",
            run.stalls
        );
    }
}
