//! The streaming tier places a file's channels as the memory tier does.
//!
//! A disk voice reads its ring at the file's own width and order, and the
//! `Place` built from the file's topology when the stream opens puts the
//! channels where the engine has them: a WAV 7.1 (rears before sides) is
//! permuted, and a narrowing with an
//! unordered end is refused, all as `AdoptError::Placement` from the adopt that
//! would have made the stream. A file already in the engine's order is read as
//! it was.
//!
//! The files are written here, as WAVE_FORMAT_EXTENSIBLE float with the
//! channel mask hand-written: channel `c` holds the constant `(c + 1) / 16`
//! (a dyadic value, so the read is exact), which makes a channel in the wrong
//! place a wrong value. The expected figures come from the engine's speaker
//! order (`ChannelTopology::default_for`), not from the placement under test.

use tutti_buffers::Planes;
use tutti_sampler::VoiceWindow;

use tutti_sampler::testing::loop_frames;

use std::path::Path;
use tutti_buffers::MaxBlock;

use tutti_buffers::FoldError;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_graph::contract;
use tutti_sampler::testing::{MockTransport, wait_for_open};
use tutti_sampler::{
    AdoptError, DiskStreamer, DiskVoice, Loop, Manual, OpenError, PlaceError, PlayDirection,
    StreamHandle, StreamReader, StreamStart,
};
use tutti_units::{Beat, Bpm, SampleRate};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
const FRAMES: u32 = 8_192;

/// The value of file channel `c`.
fn value(c: usize) -> f32 {
    (c + 1) as f32 / 16.0
}

/// A float32 `WAVE_FORMAT_EXTENSIBLE` file of [`FRAMES`] frames, `channels`
/// wide, declaring `mask`, channel `c` holding [`value`]`(c)`.
fn write_wav(path: &Path, channels: u16, mask: u32) {
    let mut fmt = Vec::new();
    fmt.extend(0xFFFEu16.to_le_bytes());
    fmt.extend(channels.to_le_bytes());
    fmt.extend((SR as u32).to_le_bytes());
    fmt.extend((SR as u32 * 4 * u32::from(channels)).to_le_bytes());
    fmt.extend((4 * channels).to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(22u16.to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    fmt.extend([
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ]);
    let mut data = Vec::new();
    for _ in 0..FRAMES {
        for c in 0..usize::from(channels) {
            data.extend(value(c).to_le_bytes());
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend((4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend((fmt.len() as u32).to_le_bytes());
    out.extend(&fmt);
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(&data);
    std::fs::write(path, out).expect("writes the file");
}

/// A hand-stepped disk streamer, and the answer to opening `path` on it (at
/// `width`, or the file's own), primed.
fn open(
    path: &Path,
    width: Option<ChannelLayout>,
) -> (
    DiskStreamer<Manual<f32>, f32>,
    Result<(StreamHandle<f32>, StreamReader<f32>), Refusal>,
) {
    let mut streamer =
        DiskStreamer::manual(SampleRate::of(SR), Default::default()).expect("builds");
    let opening = match width {
        Some(width) => streamer.open_stream_into(path, StreamStart::HEAD, width),
        None => streamer.open_stream(path, StreamStart::HEAD),
    };
    let reader = opening
        .run()
        .map_err(Refusal::Read)
        .and_then(|read| streamer.adopt(read).map_err(placed));
    if reader.is_ok() {
        assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
    }
    (streamer, reader)
}

/// Why a file did not become a stream: the read refused it, or the adopt's placement did.
#[derive(Debug, PartialEq)]
enum Refusal {
    Read(OpenError),
    Placed(PlaceError),
}

/// The placement refusal of an adopt; any other refusal is a failure of the test.
fn placed(error: AdoptError<f32>) -> Refusal {
    match error {
        AdoptError::Placement(error) => Refusal::Placed(error),
        other => panic!("expected a placement refusal, got {other:?}"),
    }
}

/// A hand-stepped streamer and the handle of the stream it streams: both are
/// held while the voice plays (dropping the handle ends the stream).
struct Rig {
    streamer: DiskStreamer<Manual<f32>, f32>,
    stream: StreamHandle<f32>,
}

/// A file written to a temp dir and streamed into a frame of `frame` channels
/// (the file's own width when `None`).
fn stream_into(
    channels: u16,
    mask: u32,
    frame: Option<ChannelLayout>,
) -> (tempfile::TempDir, Rig, StreamReader<f32>) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("placed.wav");
    write_wav(&path, channels, mask);
    let (streamer, answer) = open(&path, frame);
    let (stream, reader) = answer.expect("a placeable file opens");
    (dir, Rig { streamer, stream }, reader)
}

/// A file written to a temp dir and streamed at its own width.
fn stream(channels: u16, mask: u32) -> (tempfile::TempDir, Rig, StreamReader<f32>) {
    stream_into(channels, mask, None)
}

/// The voice that plays `reader`, placed at beat 0 for the file's length.
fn voice_of(reader: StreamReader<f32>) -> DiskVoice<f32> {
    DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)))
}

/// The last block of a few rendered from `voice`, planar: the ring is full and
/// the start-up is over.
fn settled_block(voice: DiskVoice<f32>) -> Vec<Vec<f32>> {
    let mut voice = contract::prepared(voice, SampleRate::of(SR), BLOCK);
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut last = Vec::new();
    for _ in 0..8 {
        let env = clock.env(BLOCK, SampleRate::of(SR));
        last = contract::drive_input(&mut voice, &env, Planes::default(), &[], &[]).audio;
        clock.advance(BLOCK as i64, SampleRate::of(SR));
    }
    last
}

/// Every sample of `block`'s channel `c` is exactly `want[c]`.
fn assert_channels(block: &[Vec<f32>], want: &[f32], what: &str) {
    assert_eq!(block.len(), want.len(), "{what}: width");
    for (c, (lane, &w)) in block.iter().zip(want).enumerate() {
        assert!(
            lane.iter().all(|s| s.to_bits() == w.to_bits()),
            "{what}: channel {c} is not {w}: {:?}",
            &lane[..4]
        );
    }
}

const WAV_7_1: u32 = 0x63F; // FL FR FC LFE BL BR SL SR: the rears before the sides.
const WAV_5_1: u32 = 0x3F; // FL FR FC LFE BL BR

/// A WAV-order 7.1 file plays in the engine's order: the sides before the
/// rears. The expectation is derived from the speakers, not the placement:
/// engine channel `i` plays the file channel that feeds the same speaker.
#[test]
fn a_wav_order_seven_one_is_permuted_to_the_engines_order() {
    let (_dir, _streamer, reader) = stream(8, WAV_7_1);
    let voice = voice_of(reader);
    let file = {
        use tutti_buffers::Speaker::*;
        ChannelTopology::new([
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ])
        .unwrap()
    };
    let engine = tutti_buffers::Bed::Surround71.topology();
    let want: Vec<f32> = engine
        .positions()
        .unwrap()
        .iter()
        .map(|&s| value(file.index_of(s).expect("the same speakers")))
        .collect();
    // The figures differ from the file's order, or this proves nothing.
    assert_ne!(want, (0..8).map(value).collect::<Vec<_>>());
    assert_channels(&settled_block(voice), &want, "7.1 in WAV order");
}

/// A file whose channel mask names a speaker past the standard 18 is refused
/// by the decoder when the stream is opened, at any width: the open fails with
/// `Format`.
#[test]
fn a_mask_naming_a_speaker_past_the_standard_ones_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("placed.wav");
    write_wav(&path, 2, 0x0200_0001);
    for width in [None, Some(ChannelLayout::STEREO)] {
        let (_streamer, answer) = open(&path, width);
        assert_eq!(
            answer.err(),
            Some(Refusal::Read(OpenError::Format)),
            "width {width:?}"
        );
    }
}

/// A 5.1 file into a 5-wide voice has no order to narrow by: the open is
/// refused, and a refusal leaves nothing behind, so the same file then opens at
/// stereo (a fold by position) and at its own width.
#[test]
fn a_five_one_file_into_a_five_wide_voice_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("placed.wav");
    write_wav(&path, 6, WAV_5_1);
    let (mut streamer, refused) = open(&path, Some(ChannelLayout::from(5u16)));
    let refused = refused.err();
    assert!(
        matches!(
            refused,
            Some(Refusal::Placed(PlaceError::Fold(FoldError::Unplaced {
                channel: 5,
                ..
            })))
        ),
        "{refused:?}"
    );
    // Refused again, the same way: nothing the first refusal did changed it.
    let read = streamer
        .open_stream_into(&path, StreamStart::HEAD, ChannelLayout::from(5u16))
        .run()
        .expect("the file reads");
    assert_eq!(streamer.adopt(read).err().map(placed), refused);
    let opening = streamer.open_stream_into(&path, StreamStart::HEAD, ChannelLayout::STEREO);
    let (_stereo_stream, stereo) =
        wait_for_open(&mut streamer, opening).expect("5.1 folds to stereo");
    assert_eq!(settled_block(voice_of(stereo)).len(), 2);
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (_own_stream, own) =
        wait_for_open(&mut streamer, opening).expect("and opens at its own width");
    assert_eq!(settled_block(voice_of(own)).len(), 6);
}

/// A stereo file plays as it was: the voice is two wide and every sample is the
/// file's, to the bit.
#[test]
fn a_stereo_file_is_unchanged() {
    let (_dir, _streamer, reader) = stream(2, 0x3);
    let voice = voice_of(reader);
    assert_channels(&settled_block(voice), &[value(0), value(1)], "stereo");
}

/// A fork for an offline render, which reads the file itself, places it as the
/// live voice does.
#[test]
fn a_fork_places_the_file_as_the_live_voice_does() {
    use tutti_graph::{IntoNode, Prepare, RenderMode};
    use tutti_units::Samples;
    let (_dir, _streamer, reader) = stream(8, WAV_7_1);
    let voice = voice_of(reader);
    let forked = voice
        .clone()
        .into_parts()
        .fork
        .expect("forkable")
        .fork(RenderMode::Offline)
        .expect("forks");
    let mut node = forked.node;
    let prepare = Prepare::new(SampleRate::of(SR), MaxBlock::of(Samples::new(BLOCK)))
        .with_render_mode(tutti_graph::RenderMode::Offline);
    node.prepare(&prepare);
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let env = clock.env(BLOCK, SampleRate::of(SR));
    let out =
        contract::drive_input_in(&mut *node, &prepare, &env, Planes::default(), &[], &[]).audio;
    // Engine order: FL FR FC LFE SL SR BL BR from the file's FL FR FC LFE BL BR SL SR.
    let want: Vec<f32> = [0, 1, 2, 3, 6, 7, 4, 5].map(value).to_vec();
    assert_channels(&out, &want, "the fork of a 7.1 in WAV order");
}

/// What a placed file plays in the engine's 7.1 order (FL FR FC LFE SL SR BL
/// BR), channel `c` of the file holding [`value`]`(c)`: the figures `Fold`'s
/// rules give (a speaker the destination has goes straight across; one it
/// lacks folds into its nearest at -3 dB, summed first where the file has
/// overhead speakers), written out here and not asked of `Place`.
struct Fixture {
    channels: u16,
    mask: u32,
    /// The decoder's names for the file's channels, pinned so a change in how
    /// a mask-less file is named fails here and not as a wrong figure.
    speakers: &'static [tutti_buffers::Speaker],
    want: [f32; 8],
}

fn fixtures() -> [Fixture; 3] {
    use tutti_buffers::Speaker::*;
    let m = <f32 as tutti_buffers::Float>::MINUS_3_DB;
    let v = value;
    [
        // The rears before the sides.
        Fixture {
            channels: 8,
            mask: WAV_7_1,
            speakers: &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                SideLeft,
                SideRight,
            ],
            want: [v(0), v(1), v(2), v(3), v(6), v(7), v(4), v(5)],
        },
        // No mask: WAVE's first ten speakers. The left and right of centre
        // fold into the centre, the back centre into both backs, and the right
        // side has no source.
        Fixture {
            channels: 10,
            mask: 0,
            speakers: &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                FrontLeftOfCenter,
                FrontRightOfCenter,
                BackCenter,
                SideLeft,
            ],
            want: [
                v(0),
                v(1),
                v(2) + m * v(6) + m * v(7),
                v(3),
                v(9),
                0.0,
                v(4) + m * v(8),
                v(5) + m * v(8),
            ],
        },
        // Twelve: the right side and a top centre join. The top centre (an
        // overhead speaker) folds into the front pair.
        Fixture {
            channels: 12,
            mask: 0,
            speakers: &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                FrontLeftOfCenter,
                FrontRightOfCenter,
                BackCenter,
                SideLeft,
                SideRight,
                TopCenter,
            ],
            want: [
                v(0) + m * v(11),
                v(1) + m * v(11),
                v(2) + m * (v(6) + v(7)),
                v(3),
                v(9),
                v(10),
                v(4) + m * v(8),
                v(5) + m * v(8),
            ],
        },
    ]
}

/// A file with a Fixture's shape, streamed.
fn stream_fixture(f: &Fixture) -> (tempfile::TempDir, Rig, StreamReader<f32>) {
    let (dir, rig, reader) = stream_into(f.channels, f.mask, Some(ChannelLayout::from(8u16)));
    assert_eq!(
        tutti_codec::FileIn::<f32>::open(dir.path().join("placed.wav"))
            .expect("opens")
            .channel_set()
            .topology()
            .unwrap()
            .positions()
            .unwrap(),
        f.speakers
    );
    (dir, rig, reader)
}

/// Every sample of `block` is within rounding of `want` (a fold sums in `f32`).
fn assert_near(block: &[Vec<f32>], want: &[f32; 8], what: &str) {
    assert_eq!(block.len(), 8, "{what}: width");
    for (c, (lane, &w)) in block.iter().zip(want).enumerate() {
        assert!(
            lane.iter().all(|s| (s - w).abs() < 1e-6),
            "{what}: channel {c} is not {w}: {:?}",
            &lane[..4]
        );
    }
}

/// A file wider than a frame folds into it by position (10 and 12 channels into
/// a frame of 8, which the fixtures ask for), to the figures `Fold`'s rules
/// give, and a WAV-order 7.1 is permuted.
#[test]
fn a_file_wider_than_a_frame_folds_into_it() {
    for f in fixtures() {
        let (_dir, _streamer, reader) = stream_fixture(&f);
        let voice = voice_of(reader);
        assert_near(
            &settled_block(voice),
            &f.want,
            &format!("{} channels", f.channels),
        );
    }
}

/// `n` blocks of `node` from a transport rolling from beat 0, planar.
fn render(node: &mut dyn tutti_graph::Node, clock: &MockTransport, n: usize) -> Vec<Vec<f32>> {
    let mut out: Vec<Vec<f32>> = Vec::new();
    for _ in 0..n {
        let env = clock.env(BLOCK, SampleRate::of(SR));
        let b =
            contract::drive_input_in(node, &offline_prepare(), &env, Planes::default(), &[], &[])
                .audio;
        if out.is_empty() {
            out = vec![Vec::new(); b.len()];
        }
        for (o, c) in out.iter_mut().zip(b) {
            o.extend(c);
        }
        clock.advance(BLOCK as i64, SampleRate::of(SR));
    }
    out
}

/// Fork `voice` as an export does, not yet prepared.
fn fork_unprepared(voice: &DiskVoice<f32>) -> Box<dyn tutti_graph::Node> {
    use tutti_graph::{IntoNode, RenderMode};
    voice
        .clone()
        .into_parts()
        .fork
        .expect("forkable")
        .fork(RenderMode::Offline)
        .expect("forks")
        .node
}

/// What an export prepares a fork under: offline, so the fork reads its file
/// itself and `process` may allocate.
fn offline_prepare() -> tutti_graph::Prepare {
    tutti_graph::Prepare::new(
        SampleRate::of(SR),
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(BLOCK)),
    )
    .with_render_mode(tutti_graph::RenderMode::Offline)
}

/// [`fork_unprepared`], prepared.
fn fork(voice: &DiskVoice<f32>) -> Box<dyn tutti_graph::Node> {
    let mut node = fork_unprepared(voice);
    node.prepare(&offline_prepare());
    node
}

/// Channel `c` of `direct`'s last block, all channels, as owned lanes
/// (allocating: call it outside a guard).
fn lanes<N: tutti_graph::Node>(direct: &contract::Direct<N>) -> Vec<Vec<f32>> {
    (0..8)
        .map(|c| direct.output().plane(c).samples().to_vec())
        .collect()
}

fn loop_1000_3000() -> Option<Loop> {
    Some(loop_frames(1_000, 3_000, 64))
}

/// A fork reads a placed file through every one of its paths: forward, looped
/// (the loop wrapping three times, its crossfade included), reversed, and the
/// paged read of a file wider than a frame, to the same figures as the live
/// voice.
#[test]
fn a_fork_places_every_read_path() {
    for f in fixtures() {
        for path in ["forward", "looped", "reversed"] {
            let (_dir, mut rig, reader) = stream_fixture(&f);
            if path == "looped" {
                rig.stream.looping_cell().publish(loop_1000_3000());
                assert!(rig.streamer.step_until_idle(1_000) < 1_000);
            }
            let voice = voice_of(reader);
            if path == "reversed" {
                rig.stream.direction_cell().publish(PlayDirection::Reverse);
                assert!(rig.streamer.step_until_idle(1_000) < 1_000);
            }
            let mut node = fork(&voice);
            let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
            // 6 000 frames: three trips round the 2 000-frame loop, and well
            // inside the 8 192-frame file when it is not looped.
            let out = render(&mut *node, &clock, 6_000 / BLOCK);
            let what = format!("{} channels, {path}", f.channels);
            for chunk in 0..out[0].len() / BLOCK {
                let block: Vec<Vec<f32>> = out
                    .iter()
                    .map(|l| l[chunk * BLOCK..(chunk + 1) * BLOCK].to_vec())
                    .collect();
                assert_near(&block, &f.want, &format!("{what}, block {chunk}"));
            }
        }
    }
}

/// A live placed voice allocates nothing in a block that crosses a loop edit
/// (the reader's crossfade from the old mapping, `cross_switch`) or a loop's
/// wrap, and plays its figures throughout.
#[test]
fn a_loop_edit_and_wrap_on_a_placed_stream_allocate_nothing() {
    for f in fixtures() {
        let (_dir, mut rig, reader) = stream_fixture(&f);
        let voice = voice_of(reader);
        let mut direct = contract::Direct::new(voice, SampleRate::of(SR), BLOCK);
        let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        for b in 0..90 {
            // Frame 2 816: the loop's end (3 000) is within the 256 frames a
            // change near the playhead is crossfaded over, so the reader
            // crosses a switch (`cross_switch`).
            if b == 44 {
                rig.stream.looping_cell().publish(loop_1000_3000());
            }
            direct.block_in(&clock.env(BLOCK, SampleRate::of(SR)));
            clock.advance(BLOCK as i64, SampleRate::of(SR));
            let _ = rig.streamer.step();
            if b >= 4 {
                assert_near(
                    &lanes(&direct),
                    &f.want,
                    &format!("{} channels, block {b}", f.channels),
                );
            }
        }
    }
}

/// A 7.1.4 file (twelve channels, WAV order: the backs before the sides) is
/// played at its own width, a permutation and no fold: file channel 4 and 5
/// (the backs) are the voice's 6 and 7, and the heights keep their places.
#[test]
fn a_714_file_plays_at_its_own_width_in_the_engines_order() {
    // FL FR FC LFE BL BR SL SR TFL TFR TBL TBR.
    const WAV_7_1_4: u32 = 0x2D63F;
    let (_dir, _rig, reader) = stream(12, WAV_7_1_4);
    let want: Vec<f32> = [0usize, 1, 2, 3, 6, 7, 4, 5, 8, 9, 10, 11]
        .iter()
        .map(|&c| value(c))
        .collect();
    assert_channels(&settled_block(voice_of(reader)), &want, "7.1.4 at twelve");
}

/// A file of 12 and of 16 channels (the cap) is read at its own width without
/// allocating, forward and looped, and every channel carries its figure: the
/// 7.1.4 in the engine's order (a permutation), the 16 unassigned straight.
#[test]
fn a_wide_file_at_its_own_width_is_allocation_free() {
    // FL FR FC LFE BL BR SL SR TFL TFR TBL TBR in the file; the engine's order
    // swaps the backs and sides.
    let perm = [0usize, 1, 2, 3, 6, 7, 4, 5, 8, 9, 10, 11];
    let identity: Vec<usize> = (0..16).collect();
    for (channels, mask, order) in [(12u16, 0x2D63Fu32, &perm[..]), (16, 0, &identity[..])] {
        for looped in [false, true] {
            let (_dir, mut rig, reader) = stream(channels, mask);
            if looped {
                rig.stream.looping_cell().publish(loop_1000_3000());
                assert!(rig.streamer.step_until_idle(1_000) < 1_000);
            }
            let voice = voice_of(reader);
            let mut direct = contract::Direct::new(voice, SampleRate::of(SR), BLOCK);
            let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
            direct.block_in(&clock.env(BLOCK, SampleRate::of(SR)));
            clock.advance(BLOCK as i64, SampleRate::of(SR));
            for _ in 0..80 {
                direct.block_in(&clock.env(BLOCK, SampleRate::of(SR)));
                clock.advance(BLOCK as i64, SampleRate::of(SR));
            }
            for (c, &file) in order.iter().enumerate() {
                assert!(
                    direct
                        .output()
                        .plane(c)
                        .iter()
                        .all(|s| s.to_bits() == value(file).to_bits()),
                    "{channels} channels, looped {looped}: channel {c} is not {}",
                    value(file)
                );
            }
        }
    }
}
