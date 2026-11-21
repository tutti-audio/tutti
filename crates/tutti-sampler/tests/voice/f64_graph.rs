//! A sampler node plays in an `f64` graph, in the graph's own format.
//!
//! The files are 32-bit integer WAVs whose frame `k` holds
//! `LEFT0 + 3k` (left) and `-(RIGHT0 + 5k)` (right), an integer over `2^31` as a
//! float. `LEFT0` and `RIGHT0` need 29 significant bits, which an `f32` (24)
//! cannot hold: a node that narrowed anywhere on its path reads a wrong value,
//! and the two channels differ at every frame, so a swapped channel does too.
//! The expected figures come from the integers written, not from the node.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti_buffers::Wave;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{Prepare, Standalone, TransportPosition};
use tutti_sampler::testing::wait_for_open;
use tutti_sampler::{
    DiskStreamer, DiskVoice, Manual, MemorySource, MemorySourceConfig, PlayDirection, Playback,
    SlotId, StreamStart, Voice, VoiceNode, VoicePool, VoiceSource, VoiceWindow,
};
use tutti_units::{Beat, Bpm, SampleRate, Samples};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 64;
const FRAMES: usize = 4_096;
/// The rendered span: whole blocks, well inside the file.
const RENDERED: usize = 8 * BLOCK;
/// Frames a live stream that has turned round plays through its crossfade from
/// the direction it had (the streamer records the old mapping's frames and the
/// voice blends from them); the mirror is judged past them.
const TURNED: usize = 1_024;
const TEMPO: f64 = 120.0;
const FULL_SCALE: f64 = 2_147_483_648.0;
const LEFT0: i32 = 0x2000_0001;
const RIGHT0: i32 = 0x1000_0003;

/// The integer file frame `k` holds in the left channel.
fn left(k: usize) -> i32 {
    LEFT0 + 3 * k as i32
}

/// The integer file frame `k` holds in the right channel.
fn right(k: usize) -> i32 {
    -(RIGHT0 + 5 * k as i32)
}

/// The `f64` the file holds for integer `v`.
fn value(v: i32) -> f64 {
    f64::from(v) / FULL_SCALE
}

/// The two-channel 32-bit integer file, `FRAMES` frames.
fn write_file(dir: &Path) -> PathBuf {
    let path = dir.join("wide.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("a writable temp dir");
    for k in 0..FRAMES {
        w.write_sample(left(k)).expect("writes");
        w.write_sample(right(k)).expect("writes");
    }
    w.finalize().expect("writes");
    path
}

fn prepare() -> Prepare {
    Prepare::new(SampleRate::of(RATE), MaxBlock::of(Samples::new(BLOCK)))
}

/// The graph's transport: rolling from beat 0 at `TEMPO`, the beat following
/// the frame the block starts on.
fn roll<C>(solo: &mut Standalone<C, f64>) {
    let beats_per_frame = TEMPO / 60.0 / RATE;
    let solo = solo.renderer_mut();
    solo.set_block(Samples::new(BLOCK));
    solo.set_transport_fn(move |frame| {
        TransportPosition::new(
            true,
            Bpm::of(TEMPO),
            Beat::new(frame.get() as f64 * beats_per_frame),
            None,
        )
    });
}

/// Frame `i` of `got`, from frame `skip` on, is the file's frame `file(i)`: the
/// figure for each frame is the integer over `2^31`, to within what the
/// placement's beat-to-frame arithmetic leaves of a whole position (the
/// distance between two `f32` values here is thousands of times larger).
fn assert_plays(got: &[Vec<f64>], skip: usize, file: impl Fn(usize) -> usize, what: &str) {
    assert_eq!(got.len(), 2, "{what}: two channels");
    for (i, (&l, &r)) in got[0].iter().zip(&got[1]).enumerate().skip(skip) {
        let k = file(i);
        let want = [value(left(k)), value(right(k))];
        assert!(
            (l - want[0]).abs() < 1e-12 && (r - want[1]).abs() < 1e-12,
            "{what}: frame {i} is file frame {k}: got [{l}, {r}], want {want:?}"
        );
    }
}

/// A memory source over the file, read as `f64`.
fn memory(path: &Path) -> MemorySource<f64> {
    let wave = tutti_codec::read_wave::<f64>(path).expect("the file decodes");
    MemorySource::<f64>::with_config(
        Arc::new(wave),
        MemorySourceConfig {
            channels: ChannelLayout::STEREO,
            placement: Some(VoiceWindow::from(Beat::new(0.0))),
            ..Default::default()
        },
    )
    .expect("a stereo wave places")
}

/// **A free-running `MemorySource<f64>` is a node of an `f64` graph and plays
/// the file's values exactly**: frame `i` is `LEFT0 + 3i` over `2^31`, an
/// integer position read at the wave's own rate, so exact to the bit.
#[test]
fn a_memory_source_in_an_f64_graph_plays_the_files_values_exactly() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let wave: Wave<f64> = tutti_codec::read_wave(write_file(dir.path())).expect("decodes");
    let source = MemorySource::<f64>::new(Arc::new(wave)).expect("a stereo wave places");
    source.play();
    let mut solo = Standalone::<_, f64>::new(source, prepare());
    let out = solo.render(RENDERED);
    assert_eq!(out.len(), 2);
    for (i, (&l, &r)) in out[0].iter().zip(&out[1]).enumerate() {
        assert_eq!(l, value(left(i)), "left of frame {i}");
        assert_eq!(r, value(right(i)), "right of frame {i}");
    }
    assert_eq!(out[0].len(), RENDERED);
    let wide = value(left(1));
    assert!(
        wide != f64::from(wide as f32),
        "setup: the value needs more than 24 significant bits"
    );
}

/// **A placed `VoiceNode<f64>` plays the file forward**, frame `i` the file's
/// frame `i`, in a block-by-block render.
#[test]
fn a_placed_memory_voice_in_an_f64_graph_plays_forward() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let node = VoiceNode::new(Voice {
        source: VoiceSource::Memory(memory(&write_file(dir.path()))),
        play: Playback::default(),
    })
    .expect("a finite gain and a nonempty width");
    let mut solo = Standalone::<_, f64>::new(node, prepare());
    roll(&mut solo);
    assert_plays(&solo.render(RENDERED), 0, |i| i, "forward");
}

/// **Reverse plays the mirrored values**: frame `i` of the render is the file's
/// frame `FRAMES - 1 - i`, the last frame first.
#[test]
fn a_reversed_memory_voice_in_an_f64_graph_reads_the_mirrored_frames() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let node = VoiceNode::new(Voice {
        source: VoiceSource::Memory(memory(&write_file(dir.path()))),
        play: Playback {
            direction: PlayDirection::Reverse,
            ..Playback::default()
        },
    })
    .expect("a finite gain and a nonempty width");
    let mut solo = Standalone::<_, f64>::new(node, prepare());
    roll(&mut solo);
    assert_plays(&solo.render(RENDERED), 0, |i| FRAMES - 1 - i, "reverse");
}

/// `frames` rendered a block at a time, the streamer stepped after each as a
/// running one is, so a ring that turned has its refill.
fn render_stepped<C>(
    solo: &mut Standalone<C, f64>,
    streamer: &mut DiskStreamer<Manual<f64>, f64>,
    frames: usize,
) -> Vec<Vec<f64>> {
    let mut out = vec![Vec::new(); 2];
    for _ in 0..frames / BLOCK {
        for (lane, block) in out.iter_mut().zip(solo.render(BLOCK)) {
            lane.extend(block);
        }
        let _ = streamer.step_until_idle(1_000);
    }
    out
}

/// A hand-stepped `f64` streamer and the voice over its stream of the file,
/// placed at beat 0. The stream is held with the streamer: dropping it ends it.
fn disk(
    path: &Path,
    direction: PlayDirection,
) -> (
    DiskStreamer<Manual<f64>, f64>,
    tutti_sampler::StreamHandle<f64>,
    DiskVoice<f64>,
) {
    let mut streamer =
        DiskStreamer::<Manual<f64>, f64>::manual(SampleRate::of(RATE), Default::default())
            .expect("builds");
    let opening = streamer.open_stream(path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    if direction == PlayDirection::Reverse {
        stream.direction_cell().publish(direction);
        assert!(
            streamer.step_until_idle(1_000) < 1_000,
            "the ring follows the direction"
        );
    }
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    (streamer, stream, voice)
}

/// **A `DiskVoice<f64>` is a node of an `f64` graph and plays the 32-bit file's
/// values**: through the ring, the placement and the live read, in `f64`.
#[test]
fn a_disk_voice_in_an_f64_graph_plays_the_files_values() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, _stream, voice) = disk(&write_file(dir.path()), PlayDirection::Forward);
    let mut solo = Standalone::<_, f64>::new(voice, prepare());
    roll(&mut solo);
    let got = render_stepped(&mut solo, &mut streamer, RENDERED);
    assert_plays(&got, 0, |i| i, "disk forward");
}

/// **A reversed `DiskVoice<f64>` reads the mirrored frames**: frame `i` of the
/// render is the file's frame `FRAMES - 1 - i`.
#[test]
fn a_reversed_disk_voice_in_an_f64_graph_reads_the_mirrored_frames() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, _stream, voice) = disk(&write_file(dir.path()), PlayDirection::Reverse);
    let mut solo = Standalone::<_, f64>::new(voice, prepare());
    roll(&mut solo);
    let got = render_stepped(&mut solo, &mut streamer, TURNED + RENDERED);
    assert_plays(&got, TURNED, |i| FRAMES - 1 - i, "disk reverse");
}

/// **A `VoicePool<f64>` adds a memory voice through its handle, and plays the
/// file's values** in an `f64` graph.
#[test]
fn a_pool_in_an_f64_graph_plays_a_voice_added_through_its_handle() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut solo = Standalone::<_, f64>::new(VoicePool::<f64>::new(), prepare());
    solo.controls()
        .add(
            SlotId(1),
            memory(&write_file(dir.path())),
            Playback::default(),
        )
        .expect("the add queues");
    roll(&mut solo);
    assert_plays(&solo.render(RENDERED), 0, |i| i, "pool");
}

/// **A streamed `VoiceNode<f64>` takes its stream and plays the file's
/// values**: the handle it hands back owns the stream.
#[test]
fn a_streamed_node_in_an_f64_graph_plays_the_files_values() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = write_file(dir.path());
    let mut streamer =
        DiskStreamer::<Manual<f64>, f64>::manual(SampleRate::of(RATE), Default::default())
            .expect("builds");
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    let node = VoiceNode::streaming(voice, stream, Playback::default())
        .expect("a finite gain and a nonempty width");
    let mut solo = Standalone::<_, f64>::new(node, prepare());
    roll(&mut solo);
    let got = render_stepped(&mut solo, &mut streamer, RENDERED);
    assert_plays(&got, 0, |i| i, "streamed node");
}
