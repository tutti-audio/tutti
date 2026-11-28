//! The block read is the same whatever the block length, and wherever a
//! transport change falls, bit for bit.
//!
//! A voice reads block-at-a-time (`PlaybackSlot::render_into`), and the
//! transport reaches it through each block's `Env`. A host hands the graph
//! blocks of any length, and a start, a stop or a seek lands inside a block
//! as a change in its `Env`, not at its edge. So two pools are given the same
//! voices; one renders in 256-frame blocks with every transport event inside
//! a block (a change in its `Env`), the other in 64-frame blocks with the
//! same events at block starts, on one host clock, and every output sample
//! must have the same bits.
//!
//! What it isolates is the block machinery and the clock. The two sides
//! share every kernel; a bug inside one shows on both and is the kernel's own
//! tests' (`interp`, `loop_span`, the tier tables, the vocoder's own) to catch. This
//! pins the pieces (a 256-frame block, read in one piece through scratch
//! sized from `Prepare::max_block`, as the 64-frame ones read), the
//! runs (the clock's cut at a change, a jump seen on its frame), the flush at
//! a jump, the lanes, `hermite_lanes`, the vocoder's read, the gain and the mix.

use crate::VoiceWindow;
use tutti_buffers::Interleaved;
use tutti_buffers::Planes;

use crate::testing::{loop_frames, placed_config};

use std::path::Path;
use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_buffers::MaxBlock;
use tutti_buffers::Wave;
use tutti_graph::{
    Env, IntoNode, Node, Offset, Prepare, RenderMode, TransportChanges, TransportPosition, contract,
};
use tutti_music::FrameClock;
use tutti_units::{
    Amplitude, Beat, Bpm, Cents, Frame, PlaybackRate, SampleRate, Samples, StretchFactor,
};

use super::node_config::{Placed, VoiceNodeConfig};
use super::pool::VoicePool;
use super::table::{Row, VoicePoolConfig};
use super::types::{PlayDirection, SlotId};
use crate::{DiskStreamer, DiskStreamerConfig};

const SR: f64 = 48_000.0;
const LEN: usize = 3 * 48_000;

/// Distinguishable stereo material: two partials on the left, one on the
/// right, so a swapped, dropped or folded channel shows.
fn sample(c: usize, i: usize) -> f32 {
    let t = i as f32 / SR as f32;
    match c {
        0 => (std::f32::consts::TAU * 440.0 * t).sin() * 0.4 + (i % 97) as f32 * 1e-3,
        _ => (std::f32::consts::TAU * 660.0 * t).sin() * 0.3 - (i % 89) as f32 * 1e-3,
    }
}

fn wave(channels: usize, rate: f64) -> Arc<Wave> {
    let mut w = Wave::new(channels, SampleRate::of(rate));
    for i in 0..LEN {
        let frame: Vec<f32> = (0..channels).map(|c| sample(c % 2, i + 7 * c)).collect();
        w.append(Interleaved::new(&frame, w.layout()).unwrap())
            .unwrap();
    }
    Arc::new(w)
}

fn write_wav(path: &Path) {
    let interleaved: Vec<f32> = (0..LEN)
        .flat_map(|i| [sample(0, i), sample(1, i + 7)])
        .collect();
    tutti_fixtures::signal::write_wav_f32(path, SampleRate::of(SR), 2, &interleaved);
}

/// A placed row over `w` from beat `start`: the stereo source config's gain is `gain`,
/// and `options` sets the rest of the row's node config.
fn memory(
    w: &Arc<Wave>,
    start: f64,
    gain: Amplitude,
    options: impl FnOnce(VoiceNodeConfig<f32, Placed>) -> VoiceNodeConfig<f32, Placed>,
) -> Row<f32> {
    let source = placed_config(Arc::clone(w), VoiceWindow::from(Beat::new(start))).gain(gain);
    let node = VoiceNodeConfig::placed(source).expect("a finite gain on a placed source");
    Row::from(options(node))
}

/// The host's transport: a frame clock (counted, as an engine's), rolling or
/// standing.
#[derive(Clone, Copy)]
struct Host {
    clock: FrameClock,
    playing: bool,
}

impl Host {
    fn transport(&self) -> TransportPosition {
        TransportPosition::counted(self.playing, self.clock.tempo(), self.clock.origin(), None)
    }

    fn advance(&mut self, frames: usize) {
        if self.playing {
            self.clock.advance(Samples::new(frames), None);
        }
    }
}

/// What happens to the transport at the start of a 64-frame block.
#[derive(Clone, Copy)]
enum Event {
    Seek(f64),
    Stop,
    Start,
}

fn apply(host: &mut Host, event: Event) {
    match event {
        Event::Seek(beat) => host.clock.seat(Beat::new(beat)),
        Event::Stop => host.playing = false,
        Event::Start => host.playing = true,
    }
}

fn env(frame: u64, len: usize, transport: TransportPosition, changes: TransportChanges) -> Env {
    Env {
        frame: Frame::new(frame),
        sample_rate: SampleRate::of(SR),
        block_len: Samples::new(len),
        transport,
        changes,
        play: tutti_graph::PlayClock::ZERO,
    }
}

/// The block read is the same in 256-frame blocks with the transport
/// changing inside them as in 64-frame blocks with it changing at their
/// starts, bit for bit, summed over a pool of voices covering every arm the
/// read forks on: in memory at unity, at 1.37x varispeed and half gain,
/// reversed, on a crossfaded loop, stretched 0.5x, pitched +700 cents,
/// stretched and pitched at half gain, a mono wave fanned out, a 24 kHz wave
/// on a 48 kHz clock, a six-channel wave folded to stereo, a window that
/// opens mid-render (plain, and stretched); from disk live at unity, at 0.75x,
/// and stretched 1.5x. Through a seek (a
/// re-seat, a ring jump, a stretch flush) and a stop and restart, each on a
/// frame inside a 256-frame block.
///
/// Not caught here, because both sides share it (this is a consistency
/// check): `render_into` not flushing at a jump — the voice pool's
/// `a_transport_seek_flushes_stretch_state` and
/// `a_transport_seek_flushes_a_standalone_voice_node` fail on it —
/// and the clock reporting a jump on a restart where the playhead stood,
/// which `clock`'s `a_stop_a_seek_while_stopped_and_a_restart_are_no_jump`
/// pins.
#[test]
fn the_block_read_is_the_same_at_any_block_length() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("material.wav");
    write_wav(&path);
    let stereo = wave(2, SR);
    let mono = wave(1, SR);
    let low = wave(2, 24_000.0);
    let six = wave(6, SR);

    // Disk streams: 0..3 for the long-block pool, 3..6 for the short-block one, and
    // 6..9 and 9..12 for the pools whose offline fork is rendered.
    let mut streamer = DiskStreamer::manual(SampleRate::of(SR), DiskStreamerConfig::default())
        .expect("streamer builds");
    // Each stream's handle is the streamed row's, and its voice sounds for as long
    // as the row holds it.
    let (mut streams, mut readers): (Vec<_>, Vec<_>) = (0..12)
        .map(|_| {
            let read = streamer
                .open_stream(&path, crate::StreamStart::HEAD)
                .run()
                .expect("the file reads");
            let (stream, reader) = streamer.adopt(read).expect("adopts");
            (Some(stream), Some(reader))
        })
        .unzip();
    assert!(
        streamer.step_until_idle(512) < 512,
        "the rings never primed"
    );
    // A stream's reader is its voice's, once: each voice takes its own.
    let mut disk = |stream: usize, speed: PlaybackRate, stretch: StretchFactor| {
        let reader = readers[stream]
            .take()
            .expect("each stream builds one voice");
        let mut v = crate::DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
        v.set_render(SampleRate::of(SR), RenderMode::Realtime);
        let handle = streams[stream]
            .take()
            .expect("each stream builds one voice");
        let config =
            VoiceNodeConfig::streaming(v, handle, Amplitude::UNITY, stretch, Cents::new(0.0))
                .expect("a stereo voice at a finite gain");
        Row::from(config.speed(speed))
    };

    let unity = Amplitude::UNITY;
    let mut build = |first_disk: usize, with_memory: bool| {
        let mut rows: Vec<Row<f32>> = vec![
            memory(&stereo, 0.0, unity, |n| n),
            memory(&stereo, 0.0, Amplitude::new(0.5), |n| {
                n.speed(PlaybackRate::of(1.37))
            }),
            memory(&stereo, 0.0, unity, |n| n.direction(PlayDirection::Reverse)),
            memory(&stereo, 0.0, unity, |n| {
                n.looped(loop_frames(1_000, 9_000, 256))
            }),
            memory(&stereo, 0.0, unity, |n| n.stretch(StretchFactor::new(0.5))),
            memory(&stereo, 0.0, unity, |n| n.pitch(Cents::new(700.0))),
            // The stretched arm applies the gain before the filter.
            memory(&stereo, 0.0, Amplitude::new(0.5), |n| {
                n.stretch(StretchFactor::new(1.5)).pitch(Cents::new(-300.0))
            }),
            memory(&mono, 0.0, unity, |n| n),
            memory(&low, 0.0, unity, |n| n),
            memory(&six, 0.0, unity, |n| n),
            memory(&stereo, 1.0, unity, |n| n),
            // Stretched, and silent until its window opens: the filter must
            // be fed nothing before then.
            memory(&stereo, 1.0, unity, |n| n.stretch(StretchFactor::new(0.5))),
            disk(first_disk, PlaybackRate::UNITY, StretchFactor::UNITY),
            disk(first_disk + 1, PlaybackRate::of(0.75), StretchFactor::UNITY),
            disk(first_disk + 2, PlaybackRate::UNITY, StretchFactor::new(1.5)),
        ];
        if !with_memory {
            // Only the streamed rows: what an offline fork reads from files.
            rows.drain(..rows.len() - 3);
        }
        rows.into_iter()
            .enumerate()
            .try_fold(
                VoicePoolConfig::<f32>::new(ChannelLayout::STEREO)
                    .expect("a width the sampler reads"),
                |config, (i, row)| config.row(SlotId(i as u128), row),
            )
            .expect("distinct keys within the pool's voices")
    };
    let mut long =
        crate::testing::prepared(VoicePool::from(build(0, true)), SampleRate::of(SR), 256);
    let mut short =
        crate::testing::prepared(VoicePool::from(build(3, true)), SampleRate::of(SR), 256);
    // The offline forks of two more pools of the three streamed rows, as an export takes
    // them: each reads its file itself (`DiskVoice::fork_copy`), whatever the ring holds,
    // so the pair sounds only if the forked streamed rows do.
    let mut offline = |first_disk: usize| {
        let parts = VoicePool::from(build(first_disk, false)).into_parts();
        let mut node = parts
            .fork
            .as_ref()
            .expect("a pool forks")
            .fork(RenderMode::Offline)
            .expect("a pool with streamed rows forks for an offline render")
            .node;
        node.prepare(&offline_prepare());
        (node, parts)
    };
    let (mut long_fork, _long_parts) = offline(6);
    let (mut short_fork, _short_parts) = offline(9);

    let heard = same_at_any_block_length(&mut long, &mut short, &mut streamer, None);
    assert_eq!(long.voice_count(), 15, "every row's voice arrived");
    assert_eq!(heard, [true, true], "silence proves nothing");
    let heard = same_at_any_block_length(
        &mut *long_fork,
        &mut *short_fork,
        &mut streamer,
        Some(&offline_prepare()),
    );
    assert_eq!(
        heard,
        [true, true],
        "the offline forks sound, and read the same at any block length"
    );
}

/// What an offline render prepares its nodes with: it reads files itself, so its blocks
/// may allocate.
fn offline_prepare() -> Prepare {
    Prepare::new(SampleRate::of(SR), MaxBlock::of(Samples::new(256)))
        .with_render_mode(RenderMode::Offline)
}

/// One block of `node` under `env`: inside the real-time scope unless `offline` says
/// the render is one that reads files in its blocks.
fn drive(node: &mut dyn Node, offline: Option<&Prepare>, env: &Env) -> Vec<Vec<f32>> {
    match offline {
        Some(prepare) => {
            contract::drive_input_in(node, prepare, env, Planes::default(), &[], &[]).audio
        }
        None => contract::drive_input(node, env, Planes::default(), &[], &[]).audio,
    }
}

/// Renders `long` in 256-frame blocks with the transport changing inside them and
/// `short` in 64-frame blocks with it changing at their starts, through a seek and a
/// stop and restart, and asserts every frame equal bit for bit. Which channels sounded.
fn same_at_any_block_length(
    long: &mut dyn Node,
    short: &mut dyn Node,
    streamer: &mut DiskStreamer<crate::Manual<f32>, f32>,
    offline: Option<&Prepare>,
) -> [bool; 2] {
    // Events at the start of a 64-frame block, none on a 256-frame one: in
    // the long blocks each is a change in the block's `Env`.
    let events = |b: usize| match b {
        // A seek back: every voice re-seats, the disk voices jump.
        301 => Some(Event::Seek(0.3)),
        // Stopped for a while, then rolling again where it stood.
        450 => Some(Event::Stop),
        471 => Some(Event::Start),
        _ => None,
    };
    let mut host = Host {
        clock: FrameClock::new(Beat::new(0.0), Bpm::of(120.0), SampleRate::of(SR)),
        playing: true,
    };
    let mut heard = [false; 2];
    for big in 0..150usize {
        // The long block: its first frame's transport (an event there is
        // its start), and a change in its `Env` at each event after.
        let mut walk = host;
        if let Some(e) = events(big * 4) {
            apply(&mut walk, e);
        }
        let first = walk.transport();
        let mut changes = TransportChanges::NONE;
        for k in 1..4 {
            walk.advance(64);
            if let Some(e) = events(big * 4 + k) {
                apply(&mut walk, e);
                changes
                    .push(
                        Offset::new(Samples::new(k * 64), Samples::new(256))
                            .expect("inside the block"),
                        walk.transport(),
                    )
                    .expect("a change per piece");
            }
        }
        let frame = (big * 256) as u64;
        let got = drive(long, offline, &env(frame, 256, first, changes));

        // The same span in 64-frame blocks, each event at a block's start.
        let mut want = [Vec::with_capacity(256), Vec::with_capacity(256)];
        for k in 0..4 {
            if let Some(e) = events(big * 4 + k) {
                apply(&mut host, e);
            }
            let at = frame + (k * 64) as u64;
            let out = drive(
                short,
                offline,
                &env(at, 64, host.transport(), TransportChanges::NONE),
            );
            for (w, o) in want.iter_mut().zip(out) {
                w.extend(o);
            }
            host.advance(64);
        }

        for c in 0..2 {
            for i in 0..256 {
                assert_eq!(
                    got[c][i].to_bits(),
                    want[c][i].to_bits(),
                    "block {big}, channel {c}, frame {i}: a 256-frame block read {} \
                     against 64-frame blocks' {}",
                    got[c][i],
                    want[c][i]
                );
                heard[c] |= want[c][i].abs() > 0.1;
            }
        }
        let _ = streamer.step();
    }
    heard
}
