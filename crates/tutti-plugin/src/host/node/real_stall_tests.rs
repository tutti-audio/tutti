//! The block budget, against a **real plugin that really stops**.
//!
//! `a_timed_out_reply_is_drained_rather_than_paired_with_a_later_block` proves
//! this with a mock server, and it stays: it is deterministic, needs no
//! subprocess, and can construct timings a real plugin cannot be talked
//! into. What it cannot do is prove the mock resembles the thing it stands in
//! for. A mock that sleeps before replying models a *server* that is slow; the
//! case the design is actually built for is a **plugin** that does not return
//! from `process()` — a lock it should not have taken, a disk read, a GC pause —
//! with a correct server stuck behind it.
//!
//! This suite is that: a real `plugin-server` subprocess, a real dlopen'd CLAP
//! plugin, and a `process()` that parks on an atomic until the host releases it.
//!
//! # Why the plugin spins instead of sleeping
//!
//! The switch parks on an atomic in a spin loop — no pipe, no condvar, no
//! `sleep`. The point under test is that the *host* is not waiting, so what the
//! plugin's thread does while parked is irrelevant to the measurement; a spin
//! keeps the plugin free of any synchronisation primitive whose own timing could
//! be mistaken for the host's. It also means the release is immediate and
//! unbuffered, so "the late reply arrives after the release" is a statement
//! about the host's drain rather than about a wakeup latency.
//!
//! # What is asserted, and against what
//!
//! The reply count rising after the release is the deterministic gate. Every
//! other observable here is circumstantial: the audio goes silent for a starved
//! block, but so does audio from a plugin that never loaded; the session stays
//! healthy, but so does one that was never stressed. Only a reply taken after the
//! release says *the budget elapsed, the block was abandoned, and the reply was
//! later taken back off the socket*.

use std::time::Duration;

use tutti_buffers::MaxBlock;
use tutti_buffers::Planes;
use tutti_platform::Instant;

use tutti_buffers::ChannelLayout;
use tutti_graph::{GraphBuilder, Prepare, Renderer};
use tutti_units::{SampleRate, Samples};

use crate::PluginClient;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::IpcConfig;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// Which `process()` call parks. Late enough that the pipeline is at steady
/// state, so the stall is a *mid-run* event rather than part of start-up.
const PARK_FROM: u32 = 4;

/// `probe_tag(0, 0)` — what `TagPassthrough` adds to channel 0.
const TAG_P0C0: f32 = 1.0;

/// DC fed to every input channel. Neither 0 (which silence would coincide with)
/// nor 1 (which the tag is).
const INPUT_DC: f32 = 7.0;

/// A healthy block of channel 0.
const EXPECTED_LIVE: f32 = INPUT_DC + TAG_P0C0;

use tutti_fixtures::midi;
/// Serializes these tests against each other and against every other suite that
/// drives the reference CLAP probe.
///
/// Two things need it: wall clock (each test paces itself against a real
/// subprocess, and neighbours starve each other) and the process-global
/// environment the probe's switches ride on.
///
/// The machine-wide lock directory ([`machine_lock`](super::machine_lock)),
/// shared with `process_pipeline_tests` and the integration suites.
use tutti_fixtures::{ScopedEnv, machine_lock};

/// The `plugin-server` binary these tests spawn.
///
/// # Panics
///
/// If it has not been built; the message names the command that builds it
/// (`CLAUDE.md`, "Build `plugin-server` first").
pub(super) fn plugin_server_path() -> &'static str {
    static RESOLVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            tutti_fixtures::plugin_server()
                .to_string_lossy()
                .into_owned()
        })
        .as_str()
}

/// Release a parked `process()` **in the subprocess**.
///
/// The switch is an `extern "C"` symbol in the plugin's image, and the image
/// that matters is the one the *subprocess* loaded. Calling the symbol from
/// here would `dlopen` a second, independent copy in this process and flip a
/// static nothing reads — the rlib/cdylib separate-statics trap, one process
/// further out, and it fails silently.
///
/// So the release rides the same channel the arming did: a file the plugin polls
/// while parked. A file rather than a signal or a socket because the plugin must
/// stay free of anything that could block or allocate — it is spinning on the
/// audio thread — and an `exists()` check is neither.
struct Release(tempfile::TempDir);

impl Release {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("a scratch directory"))
    }

    /// Where [`fire`](Self::fire) writes, and what the parked plugin polls.
    fn path(&self) -> std::path::PathBuf {
        self.0.path().join("release")
    }

    /// Releases the parked plugin.
    fn fire(&self) {
        std::fs::File::create(self.path()).expect("the release file is written");
    }
}

/// Load the probe into a real `plugin-server` subprocess.
fn load_probe(
    env: &mut ScopedEnv,
    render_mode: u32,
    release: &Release,
) -> (PluginClient, PluginHandle) {
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    env.set("TUTTI_CLAP_PROBE_RENDER_MODE", render_mode.to_string());
    env.set(
        "TUTTI_CLAP_PROBE_RELEASE_FILE",
        release.path().to_string_lossy().into_owned(),
    );
    let client = PluginClient::new(
        IpcConfig::at(tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source")),
        &tutti_platform::Executor::new(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_db::Loadable::uncatalogued(tutti_plugin_types::PluginRef::file(
            tutti_fixtures::clap_probe(),
        )),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("load the reference CLAP plugin through a real plugin-server");
    let handle = client.handle();
    (client, handle)
}

/// `client`, bound, as the only node of a graph rendering `BLOCK`-frame
/// blocks: the global inputs feed its inputs, its outputs the global outputs.
fn graph_of(client: PluginClient) -> (Renderer, tutti_plugin_host::PluginControls) {
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(layout(client.inputs()), layout(client.outputs()));
    let (key, controls) = g.add_with_controls(client.bind(
        tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
    ));
    g.pipe_input(key).pipe_output(key);
    let renderer = g
        .renderer(Prepare::new(
            SampleRate::of(SAMPLE_RATE),
            MaxBlock::of(Samples::new(BLOCK)),
        ))
        .expect("a one-plugin graph compiles");
    (renderer, controls)
}

/// Waits until the server has answered the chunk the last block submitted, so
/// the next block plays it: the pipeline never waits for a reply, and a block
/// run before the answer reads back silence. Not for a parked plugin, which
/// answers nothing.
fn await_answer(controls: &tutti_plugin_host::PluginControls) {
    await_answer_or(controls, "the server to answer the chunk in flight", || {
        false
    });
}

/// [`await_answer`], also ending when `or` holds: the chunk will not be answered.
fn await_answer_or(
    controls: &tutti_plugin_host::PluginControls,
    what: &str,
    mut or: impl FnMut() -> bool,
) {
    tutti_fixtures::wait_until(what, Duration::from_secs(60), || {
        controls.chunk_in_flight_answered() || or()
    });
}

/// Drive one block of DC and return output channel 0.
fn drive_block(unit: &mut Renderer, midi_out: &mut tutti_plugin_types::MidiEventVec) -> Vec<f32> {
    let _ = midi_out;
    let dc = [INPUT_DC; BLOCK];
    let inputs = unit.editor().spec().topology.inputs.count() as usize;
    let chans: Vec<&[f32]> = (0..inputs).map(|_| &dc[..]).collect();
    let out = if inputs == 0 {
        unit.render(BLOCK)
    } else {
        unit.render_input(Planes::new(&chans).unwrap())
    };
    out.into_iter().next().expect("the probe has outputs")
}

// ---------------------------------------------------------------------------

/// A plugin that stops answering for a while: the budget elapses, the block is
/// abandoned, audio keeps flowing, the session stays healthy, and the late reply
/// is drained once the plugin is released.
///
/// One test, because these are stages of one run and their **order** is the
/// content. Split apart, each would be asserting a property of a different
/// stall, and nothing would say the recovery belongs to the abandonment.
#[test]
fn a_real_plugin_that_stops_answering_is_abandoned_and_later_drained() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    env.set("TUTTI_CLAP_PROBE_BLOCK_FROM", PARK_FROM.to_string());

    // `RenderMode::TagPassthrough` (1): every output sample is input + tag, so a
    // correct block is a known constant and silence is unmistakable.
    let release = Release::new();
    let (client, handle) = load_probe(&mut env, 1, &release);
    let bridge = client.bridge();
    let (mut client, controls) = graph_of(client);
    let mut midi_out = tutti_plugin_types::MidiEventVec::new();

    // --- Steady state before the park.
    let mut good_before = 0;
    for _ in 0..(PARK_FROM as usize - 1) {
        let out = drive_block(&mut client, &mut midi_out);
        if out.iter().all(|&s| s == EXPECTED_LIVE) {
            good_before += 1;
        }
        await_answer(&controls);
    }
    assert!(
        good_before >= 1,
        "the plugin must render correctly before it parks, or every assertion \
         below is about a plugin that was never working"
    );

    // --- Drive through the park. The budget elapses on each of these.
    //
    // Audio must keep flowing: `process` returns per block, and the blocks the
    // server never answered read back as silence rather than stalling the
    // caller. That is the property the whole pipelined design exists for.
    //
    // Blocks run until `STALLED_BLOCKS` of them have been abandoned: the budget
    // is a wall-clock rule the bridge applies when it next collects, so the
    // counter is the event.
    const STALLED_BLOCKS: u64 = 6;
    let deadline = Instant::now() + Duration::from_secs(60);
    while handle.block_counts().abandoned < STALLED_BLOCKS {
        assert!(
            Instant::now() < deadline,
            "the parked plugin's blocks were never abandoned"
        );
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::yield_now();
    }

    // --- The session survived. A missed budget is not a death.
    //
    // A `Timeout` must not propagate out of `handle` into `pump`, which treats
    // every error as connection-level — one late block would crash the bridge
    // permanently and silence the plugin for the rest of the session while the
    // server was still running and still correct.
    assert!(
        !handle.status().is_dead(),
        "a plugin that missed its block budget must not be marked dead: {:?}",
        handle.status()
    );

    // --- Release, and let the drain happen.
    release.fire();
    let late_before = bridge.audio().block_counts().late;
    let mut collected = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let _ = drive_block(&mut client, &mut midi_out);
        // The released plugin works through what it was sent; the next block
        // comes once it has answered one, or an owed reply has been collected.
        await_answer_or(&controls, "the released plugin to answer", || {
            bridge.audio().block_counts().late > late_before
        });
        collected = bridge.audio().block_counts().late - late_before;
        if collected > 0 {
            break;
        }
    }

    // --- The deterministic gate.
    assert!(
        collected > 0,
        "no owed reply was collected after the release. Either the budget never elapsed (so \
         nothing was owed, and this test exercised the ordinary path rather than the \
         abandonment one), or the owed replies were never taken back off the \
         socket — which leaves the stream one frame out of step for the rest of \
         the session."
    );

    // --- And audio recovers.
    let mut good_after = 0;
    for _ in 0..8 {
        let out = drive_block(&mut client, &mut midi_out);
        if out.iter().all(|&s| s == EXPECTED_LIVE) {
            good_after += 1;
        }
        await_answer(&controls);
    }
    assert!(
        good_after >= 1,
        "audio must recover once the plugin is released. Silence here means the \
         stream never resynchronised — the classic symptom of a drained reply \
         being paired with the wrong block."
    );

    assert!(
        !handle.status().is_dead(),
        "the session must still be healthy after recovery"
    );
}

/// The counter really moves only because a block was abandoned, and an
/// ordinary run rarely abandons one.
///
/// The control for the test above, and the reason its `collected > 0` assertion is
/// worth anything. An identical run with **no** park must show:
///
/// - **`abandoned <= ORDINARY_MISSES`**: an ordinary block misses its budget
///   only occasionally. Without this, a budget that every block misses would
///   make the stall
///   test's abandonment a property of the budget rather than of the park.
///
/// The count is not pinned to 0: the budget is wall clock, and a loaded machine can
/// make an ordinary block miss it. See the bound's derivation below.
#[test]
fn an_unstressed_plugin_abandons_few_blocks_and_collects_no_more_than_it_owed() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    // No `BLOCK_FROM`: the plugin answers every block on time.

    let release = Release::new();
    let (client, handle) = load_probe(&mut env, 1, &release);
    let bridge = client.bridge();
    let (mut client, controls) = graph_of(client);
    let mut midi_out = tutti_plugin_types::MidiEventVec::new();

    const BLOCKS: usize = 12;
    for _ in 0..BLOCKS {
        let _ = drive_block(&mut client, &mut midi_out);
        await_answer(&controls);
    }

    assert!(
        !handle.status().is_dead(),
        "an unstressed plugin stays alive"
    );

    let abandoned = bridge.audio().block_counts().abandoned;
    // A quarter of the blocks. The budget is 4 periods (5.3 ms at 64/48k), the
    // probe's work is a passthrough, and each block follows the server's
    // answer to the one before. So an ordinary block is abandoned only if its
    // round trip is kept off a core for over 5 ms; idle that is 0 of 12,
    // and it stayed 0 of 12 in every run under bursty load pinned to one core.
    // A machine that keeps it off a core that long on more than 3 widely
    // spaced blocks cannot judge this control anyway.
    const ORDINARY_MISSES: u64 = BLOCKS as u64 / 4;
    assert!(
        abandoned <= ORDINARY_MISSES,
        "{abandoned} of {BLOCKS} ordinary blocks missed their budget (at most \
         {ORDINARY_MISSES} allowed). The plugin was never parked, so the \
         budget itself is being missed routinely, and the stall test's \
         abandonment would not be evidence of the park."
    );
}

/// **A plugin that never returns from `process()` shows as abandoned blocks
/// growing with no reply, and its server can be ended.** The probe parks for good from block
/// 4 (nothing releases it): the abandoned count climbs past what a host reads as a hang while the
/// reply count stands still, the plugin stays `Alive` (nothing
/// crashed), and `terminate` ends the server and latches the host's reason,
/// so `status` reads `Dead` with it.
#[test]
fn a_parked_plugin_shows_as_abandoned_blocks_with_no_replies_and_can_be_terminated() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    env.set("TUTTI_CLAP_PROBE_BLOCK_FROM", PARK_FROM.to_string());
    let release = Release::new();
    let (client, handle) = load_probe(&mut env, 1, &release);
    let (mut client, _controls) = graph_of(client);
    let mut midi_out = tutti_plugin_types::MidiEventVec::new();

    assert_eq!(
        handle.block_counts().abandoned,
        0,
        "nothing is abandoned yet"
    );
    const HANG: u64 = 16;
    let deadline = Instant::now() + Duration::from_secs(60);
    let replies_at_park = loop {
        let counts = handle.block_counts();
        if counts.abandoned >= HANG {
            break counts.replies;
        }
        assert!(
            Instant::now() < deadline,
            "abandoned blocks stopped at {}",
            counts.abandoned
        );
        let _ = drive_block(&mut client, &mut midi_out);
    };
    // Replies came before the park; none since the blocks began to be
    // abandoned (the count read at that moment is the baseline).
    for _ in 0..5 {
        let _ = drive_block(&mut client, &mut midi_out);
    }
    assert_eq!(
        handle.block_counts().replies,
        replies_at_park,
        "a parked plugin gave no reply"
    );
    assert!(
        matches!(handle.status(), tutti_plugin_host::PluginStatus::Alive),
        "a parked plugin has not crashed"
    );

    let reason = tutti_plugin_types::TerminationReason::Hung {
        blocks: HANG,
        held: Duration::from_secs(5),
    };
    handle.terminate(reason);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !handle.status().is_dead() {
        assert!(Instant::now() < deadline, "terminate left the plugin alive");
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::yield_now();
    }
    assert!(
        matches!(
            handle.status().cause(),
            Some(tutti_plugin_types::PluginFailure::Terminated { reason: r }) if *r == reason
        ),
        "the host's reason is the cause: {:?}",
        handle.status().cause()
    );
    // And the server is gone, not merely written off.
    #[cfg(target_os = "linux")]
    {
        let pid = handle.server_pid().expect("a subprocess plugin");
        tutti_fixtures::wait_until("the server to end", Duration::from_secs(60), || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                stat.rsplit(')')
                    .next()
                    .is_some_and(|rest| rest.trim_start().starts_with(['Z', 'X']))
            })
        });
    }
}

/// A 30-byte SysEx dump's five packets, and a note: what the three tests
/// below feed the node on its event input.
fn dump_and_note() -> (Vec<tutti_graph::EventKind>, tutti_graph::EventKind) {
    use tutti_ump::{MidiChannel, MidiGroup};
    let payload: Vec<u8> = (0..30).collect();
    let dump: Vec<_> = tutti_ump::Sysex7Packets::from_payload(MidiGroup::FIRST, &payload)
        .expect("a 7-bit payload")
        .collect();
    assert_eq!(dump.len(), 5);
    let note = midi::note_on(MidiChannel::FIRST, 60, 1);
    (
        dump.into_iter().map(tutti_graph::EventKind::Midi).collect(),
        tutti_graph::EventKind::Midi(note),
    )
}

/// A node of the probe on its own, bound and driven a half chunk at a time:
/// its controls and the driver. The chunk is [`BLOCK`] frames (no quantum),
/// so a block of half of it leaves the chunk in progress.
fn half_chunk_rig(
    client: PluginClient,
) -> (
    tutti_graph::contract::Direct<dyn tutti_graph::Node>,
    tutti_plugin_host::PluginControls,
) {
    let controls = client.controls();
    let node = tutti_graph::IntoNode::into_node(client.bind(
        tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
    ))
    .0;
    let mut unit = tutti_graph::contract::Direct::boxed(node, SampleRate::of(SAMPLE_RATE), BLOCK);
    unit.set_block_len(BLOCK / 2);
    (unit, controls)
}

/// One half-chunk block carrying `kinds` at its first frame.
fn feed(
    unit: &mut tutti_graph::contract::Direct<dyn tutti_graph::Node>,
    kinds: &[tutti_graph::EventKind],
) {
    let at = tutti_graph::Offset::of(Samples::new(0), Samples::new(BLOCK / 2));
    let events: Vec<tutti_graph::Event> = kinds
        .iter()
        .map(|kind| match kind {
            tutti_graph::EventKind::Midi(ump) => tutti_graph::Event::midi(at, *ump),
            _ => unreachable!("only MIDI is fed"),
        })
        .collect();
    unit.events(0, &events);
    let _ = unit.block();
}

/// **A reset drops the MIDI taken towards the next chunk, counted, and the
/// SysEx open in it stays cut past the reset.** The server keeps its
/// reassembler's open message across the node's reset, so a tail taken after
/// it would complete the message with a gap.
///
/// The dump's first three packets and a note go in a block that leaves the
/// chunk in progress; the reset counts all four, and the dump's two tail packets
/// in the next block are refused (counted, six in all) while a note after them
/// is taken (not counted).
#[test]
fn a_reset_drops_the_pending_midi_and_keeps_its_sysex_cut() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    let (client, _handle) = load_probe(&mut env, 1, &Release::new());
    let (mut unit, controls) = half_chunk_rig(client);
    let (dump, note) = dump_and_note();
    feed(&mut unit, &[dump[0], dump[1], dump[2], note]);
    assert_eq!(controls.dropped_midi_in(), 0, "held in the chunk, not lost");

    unit.node.reset();

    assert_eq!(controls.dropped_midi_in(), 4);
    feed(&mut unit, &[dump[3], dump[4], note]);
    assert_eq!(
        controls.dropped_midi_in(),
        4 + 2,
        "the tail is cut, the note is taken"
    );
}

/// **A re-prepare drops the MIDI taken towards the next chunk, counted, and
/// the SysEx open in it stays cut.** A same-rate re-prepare (a device
/// restart to another callback size) starts the pipeline over, and the next
/// chunk's `begin` would overwrite what was taken; the server does nothing
/// for a rate it already has, so its reassembler keeps the open message.
#[test]
fn a_reprepare_drops_the_pending_midi_and_keeps_its_sysex_cut() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    let (client, _handle) = load_probe(&mut env, 1, &Release::new());
    let (mut unit, controls) = half_chunk_rig(client);
    let (dump, note) = dump_and_note();
    feed(&mut unit, &[dump[0], dump[1], dump[2], note]);

    // The same rate, another callback size.
    unit.node.prepare(&Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK * 2)),
    ));

    assert_eq!(controls.dropped_midi_in(), 4);
    feed(&mut unit, &[dump[3], dump[4], note]);
    assert_eq!(
        controls.dropped_midi_in(),
        4 + 2,
        "the tail is cut, the note is taken"
    );
}

/// **The controls' MIDI count folds in the bridge thread's.** The node
/// counts what it drops itself (here the two notes a reset drops); the bridge
/// thread counts what it skips as stale or filters out, in its own cell, which
/// `Plugin::open` attaches to the node's counters. The bridge's cell is bumped
/// here directly: driving a stale skip through a real server is
/// `two_stale_blocks`'s job.
#[test]
fn the_controls_count_the_bridges_dropped_midi_too() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    let (client, _handle) = load_probe(&mut env, 1, &Release::new());
    let bridge = client.bridge();
    let (mut unit, controls) = half_chunk_rig(client);
    let (_, note) = dump_and_note();
    feed(&mut unit, &[note, note]);
    unit.node.reset();
    bridge.audio().dropped_midi_cell().writer().add(5);
    assert_eq!(controls.dropped_midi_in(), 2 + 5);
}
