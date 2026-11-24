//! Pins the out-of-process audio pipeline: output lags input by EXACTLY one
//! block, and anything else is silence rather than wrong audio.
//!
//! The failure these guard against: if a single-bus plugin's input and output
//! regions alias (`output_base == 0`), a block's input write overwrites the
//! region the output read draws from, so the host reads its own input back at
//! unity gain — a silent bypass.
//!
//! **One block of lag is the correct answer here.** The audio thread never
//! waits for a reply: it submits block N and collects block N-1, because
//! waiting sums across serially-run nodes and overruns the callback (see
//! `batcher`'s module docs). That lag is declared to PDC, so it is compensated
//! rather than heard. What must never happen is wrong audio: a wrong block, or
//! the input echoed back.
//!
//! Two properties do the work here:
//!
//! - the mock server applies an unmistakable `output = input * 2.0`, so "the
//!   plugin was bypassed" is distinguishable from "the plugin ran";
//! - the driving signal is a per-block ramp, so "one block late" is
//!   distinguishable from "two blocks late" and from "correct".
//!
//! A constant signal would separate none of those, which is why the ramp is
//! load-bearing rather than decorative.

use crate::Batcher;
use crate::IpcThread;
use crate::PluginBridge;
use tutti_fixtures::machine_lock;

/// The chunk these tests ship: the slab ceiling they build each batcher and
/// slab with, and the block they drive (so every call is one whole chunk).
const BATCH_SIZE: usize = 64;
use crate::BlockPayload;
use crate::transport::broker_pair;
use crate::{AudioReply, AudioRequest, Framed, Request, SlabLayout};
use crate::{AudioSlab, BATCH_OUTCOMES, Chunks, RING_SLOTS, Replies, SlabHandle, SlabWriter};
use std::sync::Arc;
use tutti_buffers::FloatWidth;
use tutti_buffers::{InputOutput, Planes, PlanesMut};
use tutti_fixtures::midi;
use tutti_plugin_types::{BlockStatus, ChannelLayout, ChannelTable, ChunkCounters, MidiEventVec};
use tutti_ump::MidiChannel;
use tutti_units::{SampleRate, Samples};

/// The bridge's editor loader: these peers never open an editor, and one that
/// tried is refused (the suite loads no plugin format).
fn no_editor(
    _: tutti_plugin_types::MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    plugin: &tutti_plugin_types::PluginRef,
    _: tutti_plugin_types::PluginFormat,
) -> tutti_plugin_types::IpcResult<Box<dyn tutti_plugin_types::Editor>> {
    Err(tutti_plugin_types::IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "no editor in these tests".into(),
    })
}

const CHANNELS: usize = 2;
const GAIN: f32 = 2.0;

/// The tests' side of each chunk: an empty payload, and the plugin's MIDI-out
/// kept as emitted while the chunk plays, so a test reads what the bridge
/// returned.
impl crate::Chunks for MidiEventVec {
    fn begin(&mut self, _at: usize, _chunk: usize) {
        self.clear();
    }
    fn take(&mut self, _from: usize, _n: usize, _at: usize) {}

    fn payload(&mut self, _frames: usize) -> BlockPayload {
        BlockPayload::new(tutti_units::SampleRate::SR_48K)
    }

    fn emit(&mut self, _frame: usize, event: tutti_plugin_types::WireMidi) {
        self.try_push(event)
            .expect("an emptied list holds a chunk's MIDI-out");
    }
}

/// `CHANNELS` planar channels of `BATCH_SIZE` frames: the buffers the
/// batcher is handed, as the plugin node hands them. Fixed-size arrays so a
/// test can borrow them as the batcher's slice-of-slices without allocating
/// (the no-alloc tests run the borrow inside the guard).
struct Planar([Vec<f32>; CHANNELS]);

impl Planar {
    fn new() -> Self {
        Self(std::array::from_fn(|_| vec![0.0; BATCH_SIZE]))
    }

    fn set_scalar(&mut self, ch: usize, i: usize, v: f32) {
        self.0[ch][i] = v;
    }

    fn at_scalar(&self, ch: usize, i: usize) -> f32 {
        self.0[ch][i]
    }

    fn clear(&mut self) {
        for c in &mut self.0 {
            c.fill(0.0);
        }
    }

    fn ins(&self) -> [&[f32]; CHANNELS] {
        self.0.each_ref().map(Vec::as_slice)
    }

    fn outs(&mut self) -> [&mut [f32]; CHANNELS] {
        self.0.each_mut().map(Vec::as_mut_slice)
    }
}

/// One block through `batcher`: the caller's channel slices as the views `Batcher::process`
/// takes, `frames` long.
fn drive(
    batcher: &mut Batcher,
    bridge: &PluginBridge,
    frames: usize,
    input: &[&[f32]],
    output: &mut [&mut [f32]],
    host: &mut impl Chunks,
) {
    // On the stack: the no-alloc tests run this.
    let ins: ChannelTable<&[f32]> = input.iter().map(|c| &c[..frames]).collect();
    let mut outs: ChannelTable<&mut [f32]> = output.iter_mut().map(|c| &mut c[..frames]).collect();
    let plane_in = if ins.is_empty() {
        Planes::default()
    } else {
        Planes::new(&ins).expect("equal channels")
    };
    let plane_out = if outs.is_empty() {
        PlanesMut::default()
    } else {
        PlanesMut::new(&mut outs).expect("equal channels")
    };
    let mut io = InputOutput::new(Samples::new(frames), plane_in, plane_out).expect("equal frames");
    batcher.process(bridge.audio(), &mut io, host);
}

/// `None` on a clean EOF — the host side closing the socket at teardown is
/// normal shutdown, not a failure, so it must not panic the server thread.
fn recv_host_msg<T: Framed>(stream: &interprocess::local_socket::Stream) -> Option<T> {
    use std::io::Read;
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).ok()?;
    postcard::from_bytes(&buf).ok()
}

/// The server's end of the control connection: what the host sends on it is
/// read and dropped (these servers answer blocks only).
fn serve_control(control: interprocess::local_socket::Stream) {
    std::thread::spawn(move || while recv_host_msg::<Request>(&control).is_some() {});
}

fn send_bridge_msg<T: Framed>(stream: &interprocess::local_socket::Stream, msg: &T) {
    try_send_bridge_msg(stream, msg).unwrap();
}

/// [`send_bridge_msg`] that reports a closed peer instead of panicking, for a
/// reply the test no longer waits for.
fn try_send_bridge_msg<T: Framed>(
    stream: &interprocess::local_socket::Stream,
    msg: &T,
) -> std::io::Result<()> {
    let mut stream = stream;
    crate::protocol::frame::write_frame(&mut stream, msg).map_err(std::io::Error::other)
}

/// One stereo bus per direction — the common shape, and the one most easily
/// collapsed onto offset 0. Each direction gets its own region, which makes the
/// input-echo failure structurally impossible rather than merely unlikely.
fn stereo_layout() -> SlabLayout {
    SlabLayout {
        samples_per_channel: BATCH_SIZE,
        format: FloatWidth::F32,
        slots: RING_SLOTS as u32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    }
}

/// A bridge, its mock server's thread, and the slab's input writer, which the
/// node's batcher owns in production (`SlabWriter`: one per direction).
type Rig = (
    Arc<PluginBridge>,
    IpcThread,
    std::thread::JoinHandle<()>,
    SlabWriter,
    Replies,
);

/// A bridge wired to a mock plugin server whose `ProcessAudio` step reads each
/// input channel out of the shared slab, multiplies by [`GAIN`], and writes the
/// result back in place before replying `AudioProcessed` — exactly what a real
/// gain plugin's subprocess does.
fn bridge_with_doubling_server() -> Rig {
    bridge_with_server_stall(None)
}

/// What holds a mock server's replies: the server takes one permit before
/// each reply it holds, so a test decides which replies the server answers
/// and when. A test drops its [`Permits`] sender to let the server run on.
type Stall = Option<std::sync::mpsc::Receiver<()>>;

/// The sending end of a [`Stall`], and the stall.
type Permits = (std::sync::mpsc::Sender<()>, Stall);

/// A stall and the permits that release it.
fn stalled() -> Permits {
    let (permits, stall) = std::sync::mpsc::channel();
    (permits, Some(stall))
}

/// Holds the server for a permit, when it is stalled: a plugin under load, or
/// a subprocess the scheduler has not run yet. [`WAIT_BUDGET`] only guards a
/// hang; a dropped sender releases the server for good.
fn hold_for_permit(stall: &Stall) {
    if let Some(stall) = stall {
        let _ = stall.recv_timeout(WAIT_BUDGET);
    }
}

/// As above, but the server holds each reply for a permit of `stall`.
fn bridge_with_server_stall(stall: Stall) -> Rig {
    bridge_with_server(stereo_layout(), stall, |out| out)
}

/// The mock server over `layout`: output channel `ch` is input channel
/// `route(ch)` times [`GAIN`], after the `stall` lets it.
fn bridge_with_server(layout: SlabLayout, stall: Stall, route: fn(usize) -> usize) -> Rig {
    bridge_with_server_mode(layout, stall, route, false, None)
}

/// A mock server's hold on chunk 1: it says it has the chunk (`got`), then
/// waits to be told to go on (`release`), so a test stages later chunks while
/// the server holds it.
struct ServerGate {
    got: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// As [`bridge_with_server`]; a `mute` server answers no block at all (a
/// server that drops chunks silently), and a `gate` holds chunk 1.
fn bridge_with_server_mode(
    layout: SlabLayout,
    stall: Stall,
    route: fn(usize) -> usize,
    mute: bool,
    gate: Option<ServerGate>,
) -> Rig {
    let ((audio, server_stream), (control, control_peer)) = (broker_pair(), broker_pair());

    let out_channels: usize = layout.outputs.iter().map(|l| l.count() as usize).sum();
    let (host_slab, handle) = AudioSlab::create(layout.clone()).unwrap();
    // The server side maps the SAME backing file as a view, mirroring the real
    // subprocess: audio travels in shared memory, not over the socket.
    let mut server_slab = AudioSlab::open(&handle, layout).unwrap();

    let (bridge, bridge_thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        audio,
        control,
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(crate::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        SampleRate::SR_48K,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();

    serve_control(control_peer);

    let published = Arc::clone(bridge.audio().progress());
    let server_thread = std::thread::Builder::new()
        .name("mock-doubling-server".to_string())
        .spawn(move || {
            let mut scratch = vec![0.0f32; BATCH_SIZE];
            loop {
                let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recv_host_msg::<AudioRequest>(&server_stream)
                })) {
                    Ok(Some(msg)) => msg,
                    // Clean EOF or a panicking read: the host is gone, stop.
                    Ok(None) | Err(_) => break,
                };
                let reply = match msg {
                    AudioRequest::ProcessAudio(data) => {
                        hold_for_permit(&stall);
                        if let Some(gate) = gate.as_ref().filter(|_| data.seq == 1) {
                            let _ = gate.got.send(());
                            let _ = gate.release.recv_timeout(WAIT_BUDGET);
                        }
                        if mute {
                            continue;
                        }
                        let n = data.num_samples;
                        let seq = data.seq;
                        // Mirror the real server (`AudioPipeline::process`): read
                        // this block's INPUT slot, and write into its OUTPUT slot
                        // — two disjoint regions, not one shared in place.
                        let refused = !server_slab.has_input(seq);
                        if !refused {
                            for ch in 0..out_channels {
                                let got = server_slab
                                    .read_input_into(seq, route(ch), &mut scratch[..n])
                                    .unwrap_or(0);
                                for s in scratch[..got].iter_mut() {
                                    *s *= GAIN;
                                }
                                server_slab.write(seq, ch, &scratch[..got]).unwrap();
                            }
                            // Exactly once, after the last channel. This store is
                            // the host's evidence that the slot holds this block.
                            server_slab.publish(seq);
                            // The publisher is in this process, so the waiters on
                            // the bridge's progress are told.
                            published.notify();
                        }
                        AudioReply::AudioProcessed {
                            latency_us: 0,
                            seq,
                            midi_out: Default::default(),
                            status: Default::default(),
                            refused,
                        }
                    }
                    _ => continue,
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    send_bridge_msg(&server_stream, &reply)
                }))
                .is_err()
                {
                    break;
                }
            }
        })
        .unwrap();

    (bridge, bridge_thread, server_thread, host_slab, replies)
}

/// **A sidechain input reaches the plugin.** A stereo main bus plus a mono
/// sidechain is three input ports; the batcher stages every one into the
/// matching channel of the slab's input region, so a graph edge into the
/// node's port 2 is what the plugin reads on its sidechain. The mock server
/// here outputs the sidechain (input 2) on output 0 and the main left
/// (input 0) on output 1, one chunk late.
#[test]
fn a_sidechain_port_reaches_the_plugin() {
    let _lock = machine_lock();
    let layout = SlabLayout {
        inputs: vec![ChannelLayout::STEREO, ChannelLayout::MONO],
        ..stereo_layout()
    };
    let (bridge, _thread, _server, slab, replies) =
        bridge_with_server(layout, None, |out| [2, 0][out]);
    let host_slab = Arc::clone(slab.slab());
    let mut batcher = Batcher::new(3, CHANNELS, FloatWidth::F32, BATCH_SIZE, slab, replies);
    let main = vec![1.0f32; BATCH_SIZE];
    let side = vec![5.0f32; BATCH_SIZE];
    let mut out = Planar::new();
    let mut midi_out = MidiEventVec::new();
    for block in 0..2u64 {
        out.clear();
        drive(
            &mut batcher,
            &bridge,
            BATCH_SIZE,
            &[&main, &main, &side],
            &mut out.outs(),
            &mut midi_out,
        );
        wait_for_reply(&bridge, &host_slab, block + 1, WAIT_BUDGET);
    }
    assert_eq!(
        out.at_scalar(0, 0),
        5.0 * GAIN,
        "the sidechain, on output 0"
    );
    assert_eq!(out.at_scalar(1, 0), 1.0 * GAIN, "the main bus, on output 1");
}

/// Distinct non-constant ramp per block: block `b` channel `ch` sample `i`
/// carries `1 + b*10 + ch*100 + i*0.1`. Every (block, channel, sample) value is
/// unique, so a one-block-late or echoed-input output is unmistakable. The
/// leading `+1` keeps every sample nonzero — without it `ramp_sample(0,0,0)`
/// would be `0.0` and would spuriously match a silence-filled output.
fn ramp_sample(block: usize, ch: usize, i: usize) -> f32 {
    1.0 + block as f32 * 10.0 + ch as f32 * 100.0 + i as f32 * 0.1
}

/// Drive `blocks` consecutive ramp blocks through a fresh batcher, returning
/// each block's output as `[block][channel][sample]`.
///
/// `gap` is the pause between blocks, mimicking the spacing of a real audio
/// callback, and it stays a parameter because the two regimes still probe
/// different things. With a realistic gap the server's reply has landed, so the
/// steady state is observable and the lag must be exactly one block. With no gap
/// the bridge thread may not be scheduled at all, so a block's output may
/// genuinely be absent — and the requirement becomes that the result is
/// *silence* rather than whatever the ring slot last held.
///
/// Under the original bug the same two regimes failed in two different ways (a
/// stale reply with a gap, an empty queue without), which is why both are kept.
fn drive_blocks_with_gap(blocks: usize, gap: std::time::Duration) -> Vec<Vec<Vec<f32>>> {
    let (bridge, _bridge_thread, _server, slab, replies) = bridge_with_doubling_server();

    // The two directions are separately sized and separately addressed; the
    // output region must not alias the input region.
    let layout = stereo_layout();
    assert!(
        layout.input_ring_bytes() > 0 && layout.ring_bytes(layout.output_channels()) > 0,
        "both directions must have their own region"
    );
    let host_slab = Arc::clone(slab.slab());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );

    let mut midi_out = MidiEventVec::new();
    let mut input = Planar::new();
    let mut output = Planar::new();
    let mut captured = Vec::with_capacity(blocks);

    for block in 0..blocks {
        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                input.set_scalar(ch, i, ramp_sample(block, ch, i));
            }
        }
        output.clear();

        drive(
            &mut batcher,
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut output.outs(),
            &mut midi_out,
        );

        captured.push(
            (0..CHANNELS)
                .map(|ch| (0..BATCH_SIZE).map(|i| output.at_scalar(ch, i)).collect())
                .collect(),
        );

        if !gap.is_zero() {
            wait_for_reply(&bridge, &host_slab, block as u64 + 1, gap);
        }
    }

    captured
}

/// Wait until block `seq`'s output has been published, **panicking by name if
/// the budget is spent instead**.
///
/// Replaces a bare `sleep(gap)`. The sleep encoded a *hope* — that 5 ms is
/// always enough for the mock server to reply — and on a loaded machine it is
/// not: the reply lands after the sleep, the next block reads its slot as
/// unpublished, and the test fails with "block N: got silence". That is a defect
/// in the harness's premise, not in the pipeline, and it was reproducible at
/// roughly 1 run in 8 under heavy CPU load.
///
/// Waiting on the condition rather than on the clock removed the guess for the
/// common case. What it did *not* remove is what happens when the budget runs
/// out anyway: the function returned quietly, the caller drove the next block
/// against an unpublished slot, and the failure surfaced downstream as
/// "block N: got silence (nothing was collectable for this block)" — the
/// diagnosis for a *broken pipeline*. So a slow machine was reported as a
/// pipeline defect, which is precisely the misattribution the condition-wait
/// was introduced to end. The two outcomes have to be distinguishable at the
/// point where they differ, and only this function knows which one happened.
///
/// It now panics, naming the budget and the block. That is not a stricter
/// test — the caller was going to fail either way — it is the same failure with
/// the right cause on it.
fn wait_for_reply(bridge: &PluginBridge, slab: &AudioSlab, seq: u64, budget: std::time::Duration) {
    // The server publishes, then replies, and the reply wakes the wait.
    if bridge
        .audio()
        .progress()
        .wait_until(tutti_platform::Instant::now() + budget, || {
            slab.has_output(seq)
        })
    {
        return;
    }
    panic!(
        "the mock server did not publish block {seq}'s output within {budget:?}. \
         This is a HARNESS timeout, not a pipeline defect: the assertions that \
         follow would have reported it as \"block N: got silence\", which is the \
         diagnosis for a pipeline that collected nothing. If this fires, the \
         machine is slower than the budget assumes — raise WAIT_BUDGET — or the \
         mock server thread died, which would also stall every later block."
    );
}

/// How long [`wait_for_reply`] will wait for a block's reply before declaring
/// the harness stuck.
///
/// It is a *timeout* rather than a duration that is always spent: the common
/// case returns as soon as the server publishes, in microseconds.
///
/// Generous on purpose, and it costs nothing to be. Since the wait is on a
/// condition, this bound is only reached when the reply never comes at all — a
/// dead server thread, or a pipeline that stopped publishing — and in that case
/// the test is failing regardless of how long it waited first. Sizing it to
/// "how slow could a loaded machine plausibly be?" is the wrong question: the
/// answer has no upper bound anyone can name, and every value that is not
/// generous enough converts a slow machine into a false failure. A minute is
/// far beyond any real scheduling delay for a mock server that does two memcpys,
/// and still bounded enough that a genuinely wedged run fails rather than hangs
/// a CI job. It guards a hang and asserts completion, never speed.
const WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// Classify what a block's output actually is, so a failure names the defect
/// instead of just printing two numbers.
///
/// Gained two arms with the pipelining change. "One block late" is now the
/// *correct* answer rather than a defect, and "two blocks late" and "collapsed
/// back to synchronous" became the failures worth naming — a synchronous
/// regression would otherwise show up only as an unexplained numeric mismatch.
fn diagnose(block: usize, got: &[Vec<f32>]) -> String {
    let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
    let all = |f: &dyn Fn(usize, usize) -> f32| {
        (0..CHANNELS).all(|ch| (0..BATCH_SIZE).all(|i| approx(got[ch][i], f(ch, i))))
    };

    if all(&|_, _| 0.0) {
        "silence (nothing was collectable for this block)".to_string()
    } else if all(&|ch, i| ramp_sample(block, ch, i)) {
        "this block's input at UNITY gain — the host read its own input back \
         (the plugin was bypassed)"
            .to_string()
    } else if all(&|ch, i| ramp_sample(block, ch, i) * GAIN) {
        "THIS block's processed audio — the pipeline collapsed back to \
         synchronous, which is the overrun this design exists to avoid"
            .to_string()
    } else if block > 0 && all(&|ch, i| ramp_sample(block - 1, ch, i) * GAIN) {
        "the previous block's processed audio (the expected pipelined result)".to_string()
    } else if block > 1 && all(&|ch, i| ramp_sample(block - 2, ch, i) * GAIN) {
        "TWO blocks late — the pipeline is deeper than declared, so PDC \
         under-compensates"
            .to_string()
    } else {
        format!("unrecognised: first sample {}", got[0][0])
    }
}

/// **The core contract.** Block 0 is silence (nothing is in flight yet), and
/// from then on block N's output is block N-1's input times [`GAIN`].
///
/// Asserting the lag is *exactly* one is the point: a two-block-late
/// implementation would satisfy a vaguer "the audio eventually arrives" check
/// while silently making PDC under-compensate by 1.33 ms.
#[test]
fn pipeline_output_lags_input_by_exactly_one_block() {
    let _lock = machine_lock();
    let blocks = 5;
    let captured = drive_blocks_with_gap(blocks, WAIT_BUDGET);

    assert!(
        captured[0].iter().all(|ch| ch.iter().all(|&s| s == 0.0)),
        "block 0 must be silence — nothing has been submitted yet, and emitting \
         anything else means reading a slot nobody published: {}",
        diagnose(0, &captured[0])
    );

    let mut failures = Vec::new();
    for (block, got) in captured.iter().enumerate().skip(1) {
        let ok = (0..CHANNELS).all(|ch| {
            (0..BATCH_SIZE)
                .all(|i| (got[ch][i] - ramp_sample(block - 1, ch, i) * GAIN).abs() < 1e-4)
        });
        if !ok {
            failures.push(format!("  block {block}: got {}", diagnose(block, got)));
        }
    }

    assert!(
        failures.is_empty(),
        "block N's output must be block N-1's input x{GAIN}, but:\n{}",
        failures.join("\n")
    );
}

/// The steady-state half, pinned separately so it cannot hide behind a
/// start-up failure: several blocks in, the output must still be exactly one
/// block behind — never this block's own input echoed back, never two behind.
#[test]
fn pipeline_steady_state_is_not_echoed_or_doubly_stale() {
    let _lock = machine_lock();
    let blocks = 6;
    let captured = drive_blocks_with_gap(blocks, WAIT_BUDGET);

    for (block, got) in captured.iter().enumerate().skip(2) {
        let matches = |b: usize, gain: f32| {
            (0..CHANNELS).all(|ch| {
                (0..BATCH_SIZE).all(|i| (got[ch][i] - ramp_sample(b, ch, i) * gain).abs() < 1e-4)
            })
        };

        assert!(
            !matches(block, 1.0),
            "block {block}: the host read its own input back at unity gain — \
             the plugin was bypassed entirely"
        );
        assert!(
            !matches(block, GAIN),
            "block {block}: got THIS block's processed audio — the pipeline \
             collapsed back to waiting for the reply"
        );
        assert!(
            !matches(block - 2, GAIN),
            "block {block}: got audio from two blocks ago — deeper than the \
             one block declared to PDC"
        );

        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                let expected = ramp_sample(block - 1, ch, i) * GAIN;
                assert!(
                    (got[ch][i] - expected).abs() < 1e-4,
                    "block {block} ch {ch} sample {i}: expected {expected}, got {} — {}",
                    got[ch][i],
                    diagnose(block, got)
                );
            }
        }
    }
}

/// The ring slot the host is about to read holds a **different block's real
/// audio**, and the host must emit silence rather than that audio.
///
/// This is the single test that can detect the removal of the sequence check,
/// and it is built to be detectable by construction rather than by luck.
///
/// # Why the obvious version of this test cannot work
///
/// Driving blocks back-to-back and asserting "silence or the correct block"
/// looks like it tests this, but it does not. The mock server only ever
/// publishes the block it was asked for, so the slot a missing reply leaves
/// behind is either untouched (zeros) or already correct. Deleting the sequence
/// check entirely still passes, because there is never a wrong block present to
/// read. The distinction the test claims to make — substituted silence versus a
/// stale slot — is not observable in that setup.
///
/// So the decoy is planted deliberately instead of hoped for. There is no
/// server thread and no timing: the slot's contents are set up directly, which
/// makes the outcome the same on every run and on every machine.
///
/// # The construction
///
/// Sequences start at 1, so block 0 submits seq 1 and collects nothing
/// (`expect_seq` is `None`), and block 1 submits seq 2 and collects seq 1. At
/// `RING_SLOTS == 2`, seq 1 and seq 3 share a slot. Publishing seq 3's audio
/// therefore leaves the slot that block 1 is about to read full of loud,
/// recognisable, *wrong* audio, stamped with a sequence that does not match.
///
/// A host that consults the sequence emits silence. A host that trusts the slot
/// emits [`DECOY`] and fails. The `DECOY` value is far outside the ramp's range
/// so the failure message cannot be confused with an off-by-one.
#[test]
fn a_stale_slot_holding_real_audio_still_yields_silence() {
    /// Unmistakable, and nothing the ramp or the gain can produce.
    const DECOY: f32 = -99.0;
    /// Shares a ring slot with seq 1 — the block the host collects second.
    const DECOY_SEQ: u64 = 1 + RING_SLOTS as u64;

    let (bridge, mut bridge_thread, slab, handle, replies) = bridge_with_no_server();
    let host_slab = Arc::clone(slab.slab());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );

    // The premise, asserted rather than assumed: if the ring depth changes and
    // these stop sharing a slot, the decoy lands somewhere harmless and the
    // test would silently stop testing anything.
    assert_eq!(
        DECOY_SEQ % RING_SLOTS as u64,
        1 % RING_SLOTS as u64,
        "the decoy must occupy the same ring slot as seq 1"
    );

    // The decoy is what a server would have published: written through the
    // server's end of the slab, the output writer `open` hands back.
    let mut server = AudioSlab::open(&handle, host_slab.layout()).unwrap();
    for ch in 0..CHANNELS {
        server.write(DECOY_SEQ, ch, &[DECOY; BATCH_SIZE]).unwrap();
    }
    server.publish(DECOY_SEQ);

    let mut input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();

    // Block 0 submits seq 1 and collects nothing; block 1 submits seq 2 and
    // collects seq 1 — the slot now holding the decoy.
    for block in 0..2 {
        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                input.set_scalar(ch, i, ramp_sample(block, ch, i));
            }
        }
        output.clear();
        drive(
            &mut batcher,
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut output.outs(),
            &mut midi_out,
        );

        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                let got = output.at_scalar(ch, i);
                assert_eq!(
                    got,
                    0.0,
                    "block {block} ch {ch} sample {i}: expected silence, got {got}{}",
                    if got == DECOY {
                        " — this is the decoy, so the sequence check is not being \
                         consulted and a stale ring slot is being played as though \
                         it were current audio"
                    } else {
                        ""
                    }
                );
            }
        }
    }

    // The decoy must still be sitting there: if something overwrote it, the
    // silence above proves nothing about the sequence check.
    let mut probe = [0.0f32; BATCH_SIZE];
    let got = host_slab
        .read_output_into(DECOY_SEQ, 0, &mut probe)
        .unwrap_or(0);
    assert!(
        got == BATCH_SIZE && probe.iter().all(|&s| s == DECOY),
        "the decoy was overwritten during the run, so this test no longer \
         distinguishes substituted silence from a stale slot read"
    );

    bridge_thread.shutdown();
}

/// A bridge whose connections have no server behind them: the peers are dropped
/// at once, so the bridge reads the end of its control connection and marks
/// itself crashed through its ordinary path rather than through a test-only
/// setter.
///
/// Returned deliberately without a server thread — the point is a bridge that
/// never publishes to the slab.
fn bridge_with_no_server() -> (
    Arc<PluginBridge>,
    IpcThread,
    SlabWriter,
    SlabHandle,
    Replies,
) {
    // Both peers are dropped at once: the control lane reads the end of its
    // connection and the bridge is dead.
    let (audio, _) = broker_pair();
    let (control, _) = broker_pair();

    let layout = stereo_layout();
    let (host_slab, handle) = AudioSlab::create(layout).unwrap();

    let (bridge, bridge_thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        audio,
        control,
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(crate::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        SampleRate::SR_48K,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();

    (bridge, bridge_thread, host_slab, handle, replies)
}

/// Drive `blocks` blocks through `batcher`, asserting nothing but that it runs.
/// Split out so the warm-up and the guarded region execute *identical* code —
/// if they diverged, the guarded region could take a colder path and the
/// one-shot allocations would land inside the assertion.
fn drive_one_block(
    batcher: &mut Batcher,
    bridge: &PluginBridge,
    input: &Planar,
    output: &mut Planar,
    midi_out: &mut MidiEventVec,
) {
    drive(
        batcher,
        bridge,
        BATCH_SIZE,
        &input.ins(),
        &mut output.outs(),
        midi_out,
    );
}

// The silence substitutions are audio-thread code, so they must not allocate.
//
// Both branches are reached through `Batcher::collectable` returning `None`,
// and both are new with the pipelined design — under the synchronous one the
// audio thread waited for a reply, so "nothing to collect" was not a per-block
// path. They are the branches taken whenever a plugin is late, absent or dead,
// which is exactly when the callback can least afford a malloc.
//
// # What this does and does not prove
//
// [`assert_no_alloc`] arms a *thread-local* counter, and the detection lives in
// the `RtAllocCheck` global allocator this crate's tests register (`lib.rs`). A violation calls
// `handle_alloc_error`, which aborts rather than unwinds — so a regression here
// kills the test process rather than reporting a tidy failure. That is the
// harness working, not a broken test.
//
// Registering a global allocator is process-wide, but the forbid counter is
// not: unrelated tests in this binary allocate freely, and only the closures
// below are guarded.

/// A live-but-slow plugin must not push allocations onto the audio thread.
///
/// This is the case the pipelining exists for, and the one that found the bug:
/// the payload pool held 4 entries against a 128-deep command queue, and a
/// payload is recycled only when the bridge thread dequeues its command. So the
/// audio thread outrunning a stalled bridge — the definition of this scenario —
/// starved the pool from block 5 on and allocated ~9 KiB per block thereafter,
/// exactly when the plugin was already failing to keep up.
///
/// Kept separate from the two silence tests below because it fails for a
/// different reason: those cover branches where no audio arrives at all, while
/// this one covers the path where everything is nominally working and the
/// bridge is merely behind.
#[test]
fn a_stalled_but_live_server_does_not_allocate_on_the_audio_thread() {
    // Never released: the server holds every reply while the blocks are driven.
    let (_permits, stall) = stalled();
    let (bridge, mut bridge_thread, _server, slab, replies) = bridge_with_server_stall(stall);
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );

    let mut input = Planar::new();
    let mut output = Planar::new();
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    for _ in 0..8 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }

    // Back-to-back blocks with no gap: the audio thread outruns a stalled
    // bridge thread, which is exactly the scenario the pipelining exists for.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        }
    });

    bridge_thread.shutdown();
}

/// The pipeline's counters report what the node played: a chunk in flight
/// reads unanswered until the server answers it, a block collected after the
/// answer is no miss, and a block that outruns a stalled server is counted.
#[test]
fn the_chunk_counters_report_misses_and_answers() {
    let (permits, stall) = stalled();
    let (bridge, mut bridge_thread, _server, slab, replies) = bridge_with_server_stall(stall);
    let counters = Arc::new(ChunkCounters::default());
    let answered_in = Arc::clone(slab.slab());
    counters.attach(move |seq| answered_in.has_output(seq));
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    )
    .with_counters(Arc::clone(&counters));

    let mut input = Planar::new();
    let mut output = Planar::new();
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    assert!(counters.in_flight_answered(), "nothing in flight yet");
    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    assert!(
        !counters.in_flight_answered(),
        "a chunk the stalled server has not answered read as answered"
    );

    permits.send(()).expect("the server waits for a permit");
    wait_for(&bridge, "the server never answered", || {
        counters.in_flight_answered()
    });
    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    assert_eq!(
        counters.outcomes().missed,
        tutti_units::Chunks::ZERO,
        "an answered chunk was counted missed"
    );

    // Back to back: each block collects a chunk the server is still holding.
    for _ in 0..4 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }
    assert!(
        counters.outcomes().missed > tutti_units::Chunks::ZERO,
        "blocks that outran a stalled server were not counted"
    );

    bridge_thread.shutdown();
}

/// **A chunk the server publishes after the node played its silence is counted
/// late; one it never publishes is only missed.** A server that holds
/// each chunk, driven back to back: the second block collects the first chunk
/// unanswered (a miss). Once the server has published it, the next block
/// notices, counting it late, before its ring slot is reused.
#[test]
fn a_chunk_published_after_its_miss_is_counted_late() {
    let (permits, stall) = stalled();
    let (bridge, mut bridge_thread, _server, slab, replies) = bridge_with_server_stall(stall);
    let counters = Arc::new(ChunkCounters::default());
    let host_slab = Arc::clone(slab.slab());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    )
    .with_counters(Arc::clone(&counters));
    let input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();

    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    assert_eq!(
        counters.outcomes().missed,
        tutti_units::Chunks::new(1),
        "chunk 1 was collected unanswered"
    );
    assert_eq!(
        counters.outcomes().late,
        tutti_units::Chunks::ZERO,
        "and had not been published yet"
    );

    permits.send(()).expect("the server waits for a permit");
    wait_for(&bridge, "chunk 1 never came", || host_slab.has_output(1));
    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    assert_eq!(
        counters.outcomes().late,
        tutti_units::Chunks::new(1),
        "chunk 1, published after its miss, is counted late"
    );
    assert!(counters.outcomes().late <= counters.outcomes().missed);

    bridge_thread.shutdown();
}

/// A chunk the server never publishes is a miss and never late.
#[test]
fn a_chunk_never_published_is_missed_and_never_late() {
    // Never released: the chunks are never published.
    let (_permits, stall) = stalled();
    let (bridge, mut bridge_thread, _server, slab, replies) = bridge_with_server_stall(stall);
    let counters = Arc::new(ChunkCounters::default());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    )
    .with_counters(Arc::clone(&counters));
    let input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();

    for _ in 0..6 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }
    assert_eq!(
        counters.outcomes().missed,
        tutti_units::Chunks::new(5),
        "every chunk but the first collect"
    );
    assert_eq!(
        counters.outcomes().late,
        tutti_units::Chunks::ZERO,
        "none of them was ever published"
    );

    bridge_thread.shutdown();
}

/// Counters attached to the bridge's skipped cell, as the node's bind attaches
/// them.
fn counters_on(bridge: &PluginBridge) -> Arc<ChunkCounters> {
    let counters = Arc::new(ChunkCounters::default());
    counters.attach_skipped(bridge.audio().skipped_cell());
    counters
}

/// Waits until `done`, which the bridge's progress (a response queued, a turn
/// of the bridge thread, the server's end) wakes the check of, failing after
/// [`WAIT_BUDGET`].
fn wait_for(bridge: &PluginBridge, what: &str, done: impl FnMut() -> bool) {
    let deadline = tutti_platform::Instant::now() + WAIT_BUDGET;
    assert!(
        bridge.audio().progress().wait_until(deadline, done),
        "{what}"
    );
}

/// **A chunk the server refuses is counted refused, and each resolved miss is
/// accounted for once.** The mock server holds chunk 1 until the test has
/// staged chunk 3 over its input slot, then refuses it (as the real one does
/// for a recycled slot), saying so in its reply, and publishes chunks 2 and 3.
/// Every step waits on an event: the server holding chunk 1, chunk 3 published,
/// the bridge learning chunk 1's fate.
#[test]
fn a_chunk_the_server_refuses_is_counted_refused() {
    let (got_tx, got) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (bridge, mut bridge_thread, _server, slab, replies) = bridge_with_server_mode(
        stereo_layout(),
        None,
        |out| out,
        false,
        Some(ServerGate {
            got: got_tx,
            release: release_rx,
        }),
    );
    let counters = counters_on(&bridge);
    let host_slab = Arc::clone(slab.slab());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    )
    .with_counters(Arc::clone(&counters));
    let input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();

    let mut block = || drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    block();
    got.recv_timeout(WAIT_BUDGET)
        .expect("the server holds chunk 1");
    // Blocks 2 and 3 collect chunks 1 and 2 unanswered and stage chunk 3 over
    // chunk 1's input slot.
    block();
    block();
    release.send(()).unwrap();
    wait_for(&bridge, "chunk 3 published", || host_slab.has_output(3));
    // Block 4 sees chunk 2 in the ring; chunk 1's fate is not read yet.
    block();
    wait_for(&bridge, "chunk 1's fate", || {
        bridge.audio().batch_outcome(1) == Some(crate::BatchOutcome::Refused)
    });
    // Chunk 4 is answered before block 5, so block 5's collect finds it and
    // neither of its two looks can count it late.
    wait_for(&bridge, "chunk 4 published", || host_slab.has_output(4));
    // Block 5 resolves chunk 1.
    block();

    // Chunks 1 and 2 were missed (block 5's collect finds chunk 4 answered),
    // and both fates are learned, so the four fates sum to `missed`: chunk 1
    // the scripted refusal, chunk 2 published after its miss, and the bridge
    // kept up.
    assert_eq!(
        counters.outcomes(),
        tutti_plugin_types::ChunkOutcomes {
            missed: tutti_units::Chunks::new(2),
            late: tutti_units::Chunks::new(1),
            refused: tutti_units::Chunks::new(1),
            unanswered: tutti_units::Chunks::ZERO,
            skipped: tutti_units::Chunks::ZERO,
        }
    );

    bridge_thread.shutdown();
}

/// **A chunk the server never accounts for is counted unanswered, not
/// refused.** A mute server answers no chunk. Each block waits until the bridge
/// has dealt with the last chunk (given up on it, or skipped it), then the
/// batcher runs on past the window in which a fate can still be learned.
#[test]
fn a_chunk_the_server_never_accounts_for_is_counted_unanswered() {
    let (bridge, mut bridge_thread, _server, slab, replies) =
        bridge_with_server_mode(stereo_layout(), None, |out| out, true, None);
    let counters = counters_on(&bridge);
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    )
    .with_counters(Arc::clone(&counters));
    let input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();

    for sent in 1..=2 * BATCH_OUTCOMES as u64 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        wait_for(&bridge, "the bridge to deal with the chunk", || {
            bridge.audio().block_counts().abandoned.get()
                + bridge.audio().skipped_cell().get().get()
                >= sent
        });
    }
    assert_eq!(
        counters.outcomes().refused,
        tutti_units::Chunks::ZERO,
        "the server said nothing"
    );
    assert_eq!(
        counters.outcomes().late,
        tutti_units::Chunks::ZERO,
        "nothing was published"
    );
    assert!(
        counters.outcomes().unanswered >= tutti_units::Chunks::new(1),
        "chunks the server never answered, past the window, are unanswered"
    );

    bridge_thread.shutdown();
}

/// Nothing has ever been published, so every block takes the sequence-mismatch
/// path into `silence_block`.
#[test]
fn silence_on_a_missing_reply_does_not_allocate() {
    let (bridge, mut bridge_thread, slab, _handle, replies) = bridge_with_no_server();
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );

    let mut input = Planar::new();
    let mut output = Planar::new();
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    // Warm up OUTSIDE the guard. The batcher's tick/wire storage and the
    // payload pool allocate on first use by design; the contract under test is
    // steady state, not first block.
    for _ in 0..8 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        }
    });

    // The branch under test is only meaningful if it actually ran: a bridge
    // that somehow produced audio would make the assertion above vacuous.
    assert!(
        (0..CHANNELS).all(|ch| (0..BATCH_SIZE).all(|i| output.at_scalar(ch, i) == 0.0)),
        "expected the silence branch, but the output was not silent — this test \
         no longer covers what it claims to"
    );

    bridge_thread.shutdown();
}

/// A crashed bridge must reach silence without allocating.
///
/// **This does not prove the crash short-circuit is separately covered**, and an
/// earlier version of this comment claimed it did. Deleting the `is_crashed()`
/// check in `Batcher::collectable` leaves the entire suite green: a crashed
/// bridge never publishes, so the sequence check below it returns `None` anyway
/// — the same silence, by a longer route. The check earns its place by making
/// that outcome a decision rather than a coincidence of the numbering, which is
/// a design argument rather than a tested one.
///
/// What this test does cover is real: the crashed path is the one where every
/// dispatch fails, and it must still not allocate on the audio thread. That is
/// the claim in the name, and it is the claim being checked.
#[test]
fn silence_on_a_crashed_bridge_does_not_allocate() {
    let (bridge, mut bridge_thread, slab, _handle, replies) = bridge_with_no_server();
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );

    let mut input = Planar::new();
    let mut output = Planar::new();
    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    let mut midi_out = MidiEventVec::new();

    // Drive until the failed dispatches have marked the bridge crashed, so the
    // guarded region below exercises the crash branch rather than the mismatch
    // one. Bounded rather than unbounded: if the flag never sets, the test
    // should say so instead of hanging.
    wait_for(&bridge, "the bridge never crashed", || {
        bridge.audio().is_crashed()
    });
    drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    assert!(
        bridge.audio().is_crashed(),
        "the bridge never marked itself crashed, so this test would have \
         measured the sequence-mismatch branch instead"
    );

    // Warm up again post-crash: the first block through the short-circuit may
    // still touch cold storage.
    for _ in 0..8 {
        drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            drive_one_block(&mut batcher, &bridge, &input, &mut output, &mut midi_out);
        }
    });

    assert!(
        (0..CHANNELS).all(|ch| (0..BATCH_SIZE).all(|i| output.at_scalar(ch, i) == 0.0)),
        "a crashed bridge must emit silence"
    );

    bridge_thread.shutdown();
}

/// **A reply that arrives late must cost one block, not the session.**
///
/// `dispatch::process_timeout` is documented as the budget after which the
/// bridge thread "abandons the block" — a per-block decision, and the right one:
/// a plugin that misses one deadline is slow, not dead, and the slab's sequence
/// check already turns a missing reply into silence for that block alone.
///
/// But the timeout surfaces as `IpcError::Timeout` out of `recv_reply`, and
/// `handle` returns it through `?` into `pump`, which treats *every* `handle`
/// error as connection-level: it calls `crash()`, latches the cause, drains the
/// queue with errors and **returns**, ending the thread. From then on
/// `Batcher::collectable` short-circuits on `is_crashed()` and every later block
/// is silence, permanently — for a server that is still running, still reading
/// the socket, and still publishing correct audio.
///
/// The budget is `PROCESS_TIMEOUT_PERIODS` block periods clamped to a 2 ms
/// floor. At 64 frames / 48 kHz that is 2 ms of wall clock, on a thread with no
/// scheduling priority, so on a loaded machine the reply losing that race is a
/// property of the machine rather than of the plugin. That is the mechanism
/// behind this file's intermittent failures: the run that fails is the one where
/// the mock server thread was not scheduled inside 2 ms, and the host answered
/// by declaring a working plugin dead.
///
/// The stall here is deliberately far above the budget so the timeout is
/// certain, which makes this a deterministic test of a race — the outcome does
/// not depend on the machine, only on whether one timeout is fatal.
#[test]
fn a_late_reply_does_not_permanently_crash_the_bridge() {
    let _lock = machine_lock();
    // Held until the bridge has abandoned the block, so the first block's reply
    // misses its budget however fast the machine is.
    let (permits, stall) = stalled();
    let (bridge, _bridge_thread, _server, slab, replies) = bridge_with_server_stall(stall);
    let host_slab = Arc::clone(slab.slab());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );

    let mut input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();

    for ch in 0..CHANNELS {
        for i in 0..BATCH_SIZE {
            input.set_scalar(ch, i, ramp_sample(0, ch, i));
        }
    }
    output.clear();
    drive(
        &mut batcher,
        &bridge,
        BATCH_SIZE,
        &input.ins(),
        &mut output.outs(),
        &mut midi_out,
    );

    // The bridge abandons the block at its budget, and only then does the
    // server answer: what is under test is what the host did with the missed
    // deadline, not whether the server ever answered.
    wait_for(&bridge, "the bridge never abandoned the block", || {
        bridge.audio().block_counts().abandoned >= tutti_units::Chunks::new(1)
    });
    permits.send(()).expect("the server waits for a permit");
    wait_for(&bridge, "the server never answered", || {
        host_slab.has_output(1)
    });

    assert!(
        host_slab.has_output(1),
        "the mock server never published block 1 at all — this test cannot say \
         anything about the timeout until the server has answered"
    );
    assert!(
        !bridge.audio().is_crashed(),
        "one reply that missed its per-block budget marked the bridge crashed. \
         The server is alive and published block 1's audio correctly; the host \
         has nonetheless latched a crash, so `Batcher::collectable` will return \
         `None` for every future block and this plugin is silent forever. A \
         missed block budget must cost that block, not the connection"
    );
}

/// **After a block whose reply timed out, the socket's replies stay paired with
/// the blocks that asked for them.**
///
/// Not crashing is only half the fix. Abandoning a block does not *cancel* it:
/// the server is mid-`process` and will write that frame onto the stream when it
/// finishes. Leave it there and the next command's `recv_reply` consumes the
/// abandoned block's answer, and every later block is paired with its
/// predecessor's — permanently one behind. `AudioState::hear` forwards it instead.
///
/// # Why this asserts on MIDI rather than on audio
///
/// Audio cannot detect this, and a version of this test that drove the ramp and
/// checked the one-block lag passed with `settle` deleted. That is not a weak
/// assertion, it is the design: audio travels through the shared slab in both
/// directions and is gated on the slot's own sequence number, so which *socket
/// frame* the bridge thread happened to read is irrelevant to it.
///
/// The reply carries exactly one thing the host consumes — the plugin's
/// MIDI-out — and collecting a chunk drains it (`Replies::take`). So MIDI is the
/// only observable that can tell a paired reply from a mispaired one, which
/// makes it the only honest thing to assert. Each block's reply is stamped with
/// a distinct first data word, so an off-by-one pairing names itself.
#[test]
fn a_timed_out_reply_is_drained_rather_than_paired_with_a_later_block() {
    let _lock = machine_lock();
    // One stall, held until the bridge has abandoned the block, then full speed.
    let (permits, stall) = stalled();
    let (bridge, _bridge_thread, _server, slab, replies) = bridge_with_first_block_stalled(stall);
    let host_slab = Arc::clone(slab.slab());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );

    let mut input = Planar::new();
    let mut output = Planar::new();
    let mut midi_out = MidiEventVec::new();
    let blocks = 6;
    // Which block each drained MIDI event was stamped by, per driving block.
    let mut stamps: Vec<Vec<u32>> = Vec::with_capacity(blocks);

    for block in 0..blocks {
        for ch in 0..CHANNELS {
            for i in 0..BATCH_SIZE {
                input.set_scalar(ch, i, ramp_sample(block, ch, i));
            }
        }
        output.clear();
        drive(
            &mut batcher,
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut output.outs(),
            &mut midi_out,
        );
        stamps.push(midi_out.iter().map(|e| e.ump.0[0]).collect());
        if block == 0 {
            wait_for(&bridge, "the bridge never abandoned block 1", || {
                bridge.audio().block_counts().abandoned >= tutti_units::Chunks::new(1)
            });
            permits.send(()).expect("the server waits for a permit");
        }
        wait_for_reply(&bridge, &host_slab, block as u64 + 1, WAIT_BUDGET);
    }

    assert!(
        !bridge.audio().is_crashed(),
        "the stalled first block killed the bridge, so nothing below is a \
         statement about pairing"
    );

    // **The deterministic gate.** Everything else the drain does is invisible
    // by construction (the audio it recovers is stale and rejected by the
    // slab's sequence check, and the MIDI it forwards looks like MIDI that
    // arrived on time), so this reads what the drain collected: an owed reply,
    // counted `late`. Without the drain it stays 0 whatever the stamps say.
    //
    // Block 1's reply is held past its budget, so at least that
    // one reply is abandoned on any machine; a loaded machine can make a later
    // block miss its few-ms budget too, so neither count is pinned.
    let counts = bridge.audio().block_counts();
    assert!(
        counts.abandoned >= tutti_units::Chunks::new(1),
        "block 1 was held past its budget, yet no reply was abandoned — the rig \
         did not stage the timeout this test is about",
    );
    assert!(
        counts.late >= tutti_units::Chunks::new(1),
        "the drain collected no owed reply although {} blocks were abandoned. \
         `AudioState::hear` never ran or never collected anything, and the abandoned \
         frame is still queued — which the stamps below cannot reliably detect.",
        counts.abandoned
    );

    // The server stamps block `seq`'s reply with `data[0] == seq` (not its
    // frame offset, which a late reply's collection moves to 0). Every
    // stamp the host ever drains must therefore be a sequence it actually
    // submitted, and no sequence may arrive twice — a reply left undrained
    // shows up as the same stamp reappearing behind a later block.
    let mut seen: Vec<u32> = stamps.iter().flatten().copied().collect();
    let before = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        before,
        "the same reply was drained twice: stamps {stamps:?}. A reply the bridge \
         abandoned was left on the stream and re-read behind a later block."
    );
    for &stamp in &seen {
        assert!(
            stamp >= 1 && stamp as usize <= blocks,
            "drained a reply stamped {stamp}, which is not one of the {blocks} \
             blocks this test submitted: {stamps:?}"
        );
    }

    // The abandoned block's reply must have reached the host, not merely been
    // consumed off the socket: `settle` forwards it, and dropping it silently
    // loses the plugin's MIDI for any block whose reply ran late.
    assert!(
        seen.contains(&1),
        "block 1's reply — the one whose budget expired — was drained off the \
         socket but never forwarded to the host: stamps {stamps:?}. Its audio \
         is moot, but its MIDI-out is the plugin's real output and the socket \
         is its only path."
    );
}

/// A doubling server that stalls only its **first** reply, then runs at full
/// speed — a plugin that hitches once (a page fault, a scheduler delay) rather
/// than one that is permanently slow.
///
/// A duration is the right shape here, unlike in the backlog test: this only
/// needs the first reply to miss its budget, and 120 ms against a ~2 ms budget
/// is not a race on any machine.
fn bridge_with_first_block_stalled(stall: Stall) -> Rig {
    stamped_server(stall, true, SampleRate::SR_48K)
}

/// The doubling server of [`bridge_with_server_stall`], with two additions the
/// timeout tests need.
///
/// `first_only` stalls just the opening block rather than every one — a plugin
/// that hitches once versus one that is permanently behind, which are different
/// properties and need different rigs.
///
/// Every reply's MIDI-out carries one event stamped with the sequence it
/// answers. The reply is the only thing that crosses the socket which the host
/// consumes at all — audio travels through the slab and is gated on the slot's
/// own sequence — so this stamp is the only way a test can observe *which block*
/// an arriving frame belongs to. `offset` is a label here and nothing
/// interprets it as a position.
fn stamped_server(stall: Stall, first_only: bool, rate: SampleRate) -> Rig {
    let ((audio, server_stream), (control, control_peer)) = (broker_pair(), broker_pair());

    let layout = stereo_layout();
    let (host_slab, handle) = AudioSlab::create(layout.clone()).unwrap();
    let mut server_slab = AudioSlab::open(&handle, layout).unwrap();

    let (bridge, bridge_thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        audio,
        control,
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(crate::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        rate,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();

    serve_control(control_peer);

    let published = Arc::clone(bridge.audio().progress());
    let server_thread = std::thread::Builder::new()
        .name("mock-stamped-server".to_string())
        .spawn(move || {
            let mut scratch = vec![0.0f32; BATCH_SIZE];
            let mut stalled_once = false;
            loop {
                let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recv_host_msg::<AudioRequest>(&server_stream)
                })) {
                    Ok(Some(msg)) => msg,
                    Ok(None) | Err(_) => break,
                };
                let reply = match msg {
                    AudioRequest::ProcessAudio(data) => {
                        if !(first_only && stalled_once) {
                            stalled_once = true;
                            hold_for_permit(&stall);
                        }
                        let n = data.num_samples;
                        let seq = data.seq;
                        if server_slab.has_input(seq) {
                            for ch in 0..CHANNELS {
                                let got = server_slab
                                    .read_input_into(seq, ch, &mut scratch[..n])
                                    .unwrap_or(0);
                                for s in scratch[..got].iter_mut() {
                                    *s *= GAIN;
                                }
                                server_slab.write(seq, ch, &scratch[..got]).unwrap();
                            }
                            server_slab.publish(seq);
                            // The publisher is in this process, so the waiters on
                            // the bridge's progress are told.
                            published.notify();
                        }
                        let midi_out = [tutti_plugin_types::WireMidi {
                            offset: 0,
                            ump: tutti_ump::Ump([seq as u32, 0, 0, 0]),
                        }]
                        .into_iter()
                        .collect();
                        AudioReply::AudioProcessed {
                            latency_us: 0,
                            seq,
                            midi_out,
                            status: status_for(seq),
                            refused: false,
                        }
                    }
                    _ => continue,
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    send_bridge_msg(&server_stream, &reply)
                }))
                .is_err()
                {
                    break;
                }
            }
        })
        .unwrap();

    (bridge, bridge_thread, server_thread, host_slab, replies)
}

/// What the stamped server says of its block `seq`: a distinct status for each
/// of three consecutive blocks, so a mispairing names itself.
fn status_for(seq: u64) -> BlockStatus {
    match seq % 3 {
        0 => BlockStatus::Sleep,
        1 => BlockStatus::Continue,
        _ => BlockStatus::Tail,
    }
}

/// A host that records the status each collected chunk was handed.
#[derive(Default)]
struct StatusProbe {
    said: Vec<BlockStatus>,
}

impl crate::Chunks for StatusProbe {
    fn begin(&mut self, _at: usize, _chunk: usize) {}
    fn take(&mut self, _from: usize, _n: usize, _at: usize) {}

    fn payload(&mut self, _frames: usize) -> BlockPayload {
        BlockPayload::new(tutti_units::SampleRate::SR_48K)
    }

    fn status(&mut self, status: BlockStatus) {
        self.said.push(status);
    }

    fn emit(&mut self, _frame: usize, _event: tutti_plugin_types::WireMidi) {}
}

/// **The status a server replies with crosses the wire and reaches the host
/// with the chunk it belongs to**. The server says a
/// different status for each of blocks 1, 2 and 3 (continue, tail, sleep);
/// the batcher collects block `n` as it starts chunk `n + 1`, and hands the
/// host block `n`'s status then: nothing's before any chunk is in flight
/// (continue), then each block's own, never a neighbour's.
#[test]
fn a_blocks_status_reaches_the_host_with_its_chunk() {
    let _lock = machine_lock();
    // The bridge's per-block reply budget is four block periods. At 4 kHz a
    // 64-frame block is 16 ms long, so the budget is 64 ms: a loaded machine
    // does not abandon a block, which is not what this test is about.
    let (bridge, _thread, _server, slab, replies) =
        stamped_server(None, true, SampleRate::new(4_000.0).unwrap());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );
    let silence = vec![0.0f32; BATCH_SIZE];
    let mut out = Planar::new();
    let mut host = StatusProbe::default();
    for block in 1..=4u64 {
        out.clear();
        drive(
            &mut batcher,
            &bridge,
            BATCH_SIZE,
            &[&silence, &silence],
            &mut out.outs(),
            &mut host,
        );
        // The bridge thread counts a reply once it is available to the next
        // collect: wait for it.
        let deadline = tutti_platform::Instant::now() + WAIT_BUDGET;
        assert!(
            bridge.audio().progress().wait_until(deadline, || bridge
                .audio()
                .block_counts()
                .replies
                .get()
                >= block),
            "block {block}'s reply never arrived within {WAIT_BUDGET:?} \
             ({} blocks abandoned, {} replies); a block the bridge abandoned \
             on its reply budget carries no status",
            bridge.audio().block_counts().abandoned,
            bridge.audio().block_counts().replies,
        );
    }
    assert_eq!(
        host.said,
        [
            BlockStatus::Continue,
            status_for(1),
            status_for(2),
            status_for(3)
        ]
    );
}

/// A host whose every chunk carries `midi` MIDI events, recording what the
/// batcher reports lost: chunks not sent ([`Chunks::unsent`]) and the MIDI
/// of chunks the bridge refused ([`Chunks::refused`]).
///
/// [`Chunks::unsent`]: crate::Chunks::unsent
/// [`Chunks::refused`]: crate::Chunks::refused
#[derive(Default)]
struct LossProbe {
    midi: usize,
    unsent: usize,
    refused: Vec<usize>,
}

impl crate::Chunks for LossProbe {
    fn begin(&mut self, _at: usize, _chunk: usize) {}
    fn take(&mut self, _from: usize, _n: usize, _at: usize) {}

    fn payload(&mut self, _frames: usize) -> BlockPayload {
        let note = tutti_plugin_types::WireMidi {
            offset: 0,
            ump: midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000),
        };
        BlockPayload {
            midi: std::iter::repeat_n(note, self.midi).collect(),
            ..BlockPayload::new(tutti_units::SampleRate::SR_48K)
        }
    }

    fn unsent(&mut self) {
        self.unsent += 1;
    }

    fn refused(&mut self, midi: usize) {
        self.refused.push(midi);
    }

    fn emit(&mut self, _frame: usize, _event: tutti_plugin_types::WireMidi) {}
}

/// **A chunk whose audio cannot be staged reports its MIDI lost**, so the
/// plugin node counts it and cuts the SysEx open in it. A batcher wider
/// than the slab fails to stage its third channel on every chunk.
#[test]
fn an_unstaged_chunk_reports_its_midi_lost() {
    let _lock = machine_lock();
    let (bridge, _thread, _server, slab, replies) = bridge_with_doubling_server();
    let mut batcher = Batcher::new(3, CHANNELS, FloatWidth::F32, BATCH_SIZE, slab, replies);
    let silence = vec![0.0f32; BATCH_SIZE];
    let mut out = Planar::new();
    let mut host = LossProbe {
        midi: 4,
        ..LossProbe::default()
    };
    for _ in 0..3 {
        drive(
            &mut batcher,
            &bridge,
            BATCH_SIZE,
            &[&silence, &silence, &silence],
            &mut out.outs(),
            &mut host,
        );
    }
    assert_eq!(host.unsent, 3);
    assert!(host.refused.is_empty());
}

/// **A chunk the bridge refuses reports its MIDI lost, with how many events
/// it carried.** A bridge whose server is gone is crashed, and refuses
/// every chunk.
#[test]
fn a_refused_chunk_reports_its_midi_lost() {
    let _lock = machine_lock();
    let ((audio, _), (control, _)) = (broker_pair(), broker_pair());
    let (slab, _handle) = AudioSlab::create(stereo_layout()).unwrap();
    let (bridge, _thread, replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        audio,
        control,
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(crate::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        SampleRate::SR_48K,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();
    wait_for(&bridge, "never crashed", || bridge.audio().is_crashed());
    let mut batcher = Batcher::new(
        CHANNELS,
        CHANNELS,
        FloatWidth::F32,
        BATCH_SIZE,
        slab,
        replies,
    );
    let input = Planar::new();
    let mut out = Planar::new();
    let mut host = LossProbe {
        midi: 7,
        ..LossProbe::default()
    };
    for _ in 0..2 {
        drive(
            &mut batcher,
            &bridge,
            BATCH_SIZE,
            &input.ins(),
            &mut out.outs(),
            &mut host,
        );
    }
    assert_eq!(host.refused, vec![7, 7]);
    assert_eq!(host.unsent, 0);
}

/// **A block the bridge thread skips as stale never lets the SysEx open in
/// it through with a gap.** The mock server holds its answer to a first block
/// until four more are queued, so the bridge thread finds the first two stale
/// (`RING_SLOTS` = 2) and skips them: their MIDI is lost, counted, and the
/// rest of the message open in them (in block 3) is not sent either. The
/// message after it, and a note, arrive.
#[test]
fn a_stale_block_never_lets_a_sysex_through_with_a_gap() {
    let _lock = machine_lock();
    let cut = sysex_fragments(60); // 10 packets
    let next = sysex_fragments(12); // 2 packets
    let note = tutti_plugin_types::WireMidi {
        offset: 0,
        ump: midi::note_on(MidiChannel::FIRST, 60, 1),
    };
    let (three, four, dropped, skipped) = two_stale_blocks([
        cut[..3].to_vec(),
        cut[3..6].to_vec(),
        cut[6..]
            .iter()
            .chain(&next)
            .copied()
            .chain([note])
            .collect(),
        vec![note],
    ]);
    let mut expected_three = next.clone();
    expected_three.push(note);
    assert_eq!(three, expected_three, "the cut message's tail is not sent");
    assert_eq!(four, vec![note]);
    assert_eq!(
        dropped,
        (3 + 3 + 4) as u64,
        "blocks 1 and 2, and the cut message's tail"
    );
    assert_eq!(skipped, 2, "blocks 1 and 2 are skipped as stale");
}

/// **A stale block holding only a message's `START` cuts that message.**
/// The bridge must record the `START` as open before it marks the block's
/// open messages cut, or the message's `END` in the next block sent passes.
#[test]
fn a_stale_block_holding_only_a_start_cuts_its_message() {
    let _lock = machine_lock();
    let lost = sysex_fragments(12); // a START and an END
    let other = sysex_fragments(20);
    let note = tutti_plugin_types::WireMidi {
        offset: 0,
        ump: midi::note_on(MidiChannel::FIRST, 60, 1),
    };
    // The `START` in the last stale block: an earlier one's `lost` would
    // cut it anyway.
    let (three, four, dropped, skipped) = two_stale_blocks([
        vec![note],
        vec![lost[0]],
        [lost[1]].into_iter().chain(other.iter().copied()).collect(),
        vec![note],
    ]);
    assert_eq!(three, other, "the lost START's END is not sent");
    assert_eq!(four, vec![note]);
    assert_eq!(dropped, 3, "blocks 1 and 2, and the END");
    assert_eq!(skipped, 2, "blocks 1 and 2 are skipped as stale");
}

/// `len` payload bytes as SysEx7 packets on group 1.
fn sysex_fragments(len: usize) -> Vec<tutti_plugin_types::WireMidi> {
    let payload: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
    let mut out = Vec::new();
    out.extend(
        tutti_ump::Sysex7Packets::from_payload(tutti_ump::MidiGroup::FIRST, &payload)
            .expect("a 7-bit payload"),
    );
    out.into_iter()
        .map(|ump| tutti_plugin_types::WireMidi { offset: 0, ump })
        .collect()
}

/// Submits four blocks of MIDI while the bridge thread waits on the mock
/// server's answer to an empty first block (block 1), so the bridge thread finds
/// the first two of the four (blocks 2 and 3) stale (`RING_SLOTS` = 2) and skips
/// them. Returns the MIDI the server got in blocks 4 and 5, and the
/// bridge's count of the MIDI it did not send.
fn two_stale_blocks(
    blocks: [Vec<tutti_plugin_types::WireMidi>; 4],
) -> (
    Vec<tutti_plugin_types::WireMidi>,
    Vec<tutti_plugin_types::WireMidi>,
    u64,
    u64,
) {
    assert_eq!(RING_SLOTS, 2, "blocks 2 and 3 of 2..=5 are stale");
    let ((audio, server_stream), (control, control_peer)) = (broker_pair(), broker_pair());
    let (bridge, thread, _replies) = PluginBridge::new(
        tutti_fixtures::executor(),
        audio,
        control,
        tutti_plugin_types::PluginRef::file("test.vst3"),
        tutti_plugin_types::PluginFormat::Vst3,
        Some(crate::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        SampleRate::SR_48K,
        Arc::new(crate::ServerWatch::new()),
        tutti_plugin_types::Timeouts::default(),
    )
    .unwrap();
    // Block 1 is waited on until the server answers it, however long that takes.
    bridge.audio().set_reply_wait(Some(WAIT_BUDGET));
    serve_control(control_peer);
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (got_tx, got) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        while let Some(msg) = recv_host_msg::<AudioRequest>(&server_stream) {
            if let AudioRequest::ProcessAudio(data) = msg {
                let midi: Vec<tutti_plugin_types::WireMidi> = data.midi_events.to_vec();
                let _ = got_tx.send((data.seq, midi));
                if data.seq == 1 {
                    released.recv().unwrap();
                }
                // The test asserts on `got`, which is sent above: once it has
                // block 5 it drops the bridge, so the last reply may find the
                // peer closed. That ends the server, as a closed read does.
                let reply = AudioReply::AudioProcessed {
                    latency_us: 0,
                    seq: data.seq,
                    midi_out: Default::default(),
                    status: Default::default(),
                    refused: false,
                };
                if try_send_bridge_msg(&server_stream, &reply).is_err() {
                    break;
                }
            }
        }
    });

    let submit = |seq: u64, midi: &[tutti_plugin_types::WireMidi]| {
        let submitted = bridge.audio().submit(
            seq,
            BATCH_SIZE,
            midi.iter().copied().collect(),
            &mut tutti_plugin_types::ParameterChanges::new(),
            Default::default(),
            Default::default(),
            tutti_plugin_types::TransportInfo::new(tutti_units::SampleRate::SR_48K),
            Default::default(),
        );
        assert!(submitted.accepted());
    };
    submit(1, &[]);
    let wait = WAIT_BUDGET;
    let (seq, _) = got.recv_timeout(wait).expect("block 1 sent");
    assert_eq!(seq, 1);
    for (i, midi) in blocks.iter().enumerate() {
        submit(i as u64 + 2, midi);
    }
    release.send(()).unwrap();
    let (seq, three) = got.recv_timeout(wait).expect("block 4 sent");
    assert_eq!(seq, 4);
    let (seq, four) = got.recv_timeout(wait).expect("block 5 sent");
    assert_eq!(seq, 5);
    let dropped = bridge.audio().dropped_midi_cell().get();
    let skipped = bridge.audio().skipped_cell().get().get();
    drop(thread);
    drop(bridge);
    server.join().unwrap();
    (three, four, dropped, skipped)
}
