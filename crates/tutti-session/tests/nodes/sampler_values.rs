//! A sampler voice's steering values written with the session's verbs: its
//! window (`clip.window()`), loop (`clip.looping()`) and direction
//! (`clip.direction()`) are staged at `set` and published by the next
//! `update()`.

use tutti_buffers::{ChannelLayout, Interleaved, InterleavedMut, Wave};
use tutti_graph::NodeKey;
use tutti_sampler::{Clip, PlayError, VoiceWindow};
use tutti_session::{OutPort, Root, Session, ValueErrorKind};
use tutti_units::{Amplitude, Beat, SampleRate};

const RATE: f64 = 48_000.0;

/// Frames a beat takes at the session's 120 BPM and 48 kHz.
const BEAT: usize = 24_000;

fn offline() -> Session {
    Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds")
}

/// One second of a constant mono wave.
fn level(value: f32) -> Wave {
    let mut wave = Wave::new(1, SampleRate::of(RATE));
    for _ in 0..RATE as usize {
        wave.append(Interleaved::new(&[value], wave.layout()).unwrap())
            .unwrap();
    }
    wave
}

/// `frames` stereo frames of the session, in 512-frame blocks, the transport
/// as it stands.
fn render(s: &Session, frames: usize) -> Vec<f32> {
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 512 * 2];
    while out.len() < frames * 2 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

/// The first frame a stereo render is not silent on.
fn first_sound(out: &[f32]) -> Option<usize> {
    out.iter().position(|&x| x != 0.0).map(|i| i / 2)
}

fn wire(s: &mut Session, node: NodeKey) {
    for port in 0..2 {
        s.graph()
            .set_sources_hard(Root::output(port), [OutPort::new(node, port)])
            .unwrap();
    }
}

/// A clip of constant level placed at `at`, wired to the output.
fn placed(
    s: &mut Session,
    at: Beat,
) -> (
    tutti_session::Key<Clip<f32>>,
    tutti_sampler::VoiceNodeHandle<f32>,
) {
    let clip = Clip::new(level(0.5))
        .and_then(|c| c.channels(ChannelLayout::STEREO))
        .expect("a placeable wave")
        .at(at)
        .gain(Amplitude::new(1.0));
    let (key, handle) = s.graph().add(clip);
    wire(s, key.erase());
    let _ = s.update();
    (key, handle)
}

/// **A window set is pending until the update, then a placed voice sounds at
/// the window's start**: the clip is at beat 2 (frame 48 000); a window at
/// beat 1 is written. A read answers it and the node's cell holds beat 2; a
/// render before the update is silent over the first 40 000 frames; after the
/// update the first nonzero frame is the one of beat 1 at 120 BPM and 48 kHz,
/// 24 000.
#[test]
fn a_window_set_is_pending_until_the_update_then_places_the_voice() {
    let mut s = offline();
    let (clip, handle) = placed(&mut s, Beat(2.0));
    let beat_one = Some(VoiceWindow::from(Beat(1.0)));

    s.set(clip.window().to(beat_one)).unwrap();
    assert_eq!(
        s.get(clip.window()).unwrap(),
        beat_one,
        "the pending window"
    );
    assert_eq!(
        *handle.window_cell().current(),
        Some(VoiceWindow::from(Beat(2.0))),
        "the node's cell holds the window in force"
    );
    s.play().expect("room");
    assert_eq!(
        first_sound(&render(&s, 40_000)),
        None,
        "nothing before the update"
    );

    let mut s = offline();
    let (clip, handle) = placed(&mut s, Beat(2.0));
    s.set(clip.window().to(beat_one)).unwrap();
    let _ = s.update();
    assert_eq!(*handle.window_cell().current(), beat_one);
    s.play().expect("room");
    assert_eq!(first_sound(&render(&s, 40_000)), Some(BEAT));
}

/// File frames of the ramp the free-running test plays. Frame `i` holds
/// `i / 2^17`, exact in `f32`, so an output sample names the source frame it
/// was read from.
const RAMP_LEN: usize = 6_000;

fn ramp_value(i: usize) -> f32 {
    i as f32 / 131_072.0
}

/// A clip of the ramp at beat 0, wired to the output.
fn placed_ramp(
    s: &mut Session,
) -> (
    tutti_session::Key<Clip<f32>>,
    tutti_sampler::VoiceNodeHandle<f32>,
) {
    let mut wave = Wave::new(1, SampleRate::of(RATE));
    for i in 0..RAMP_LEN {
        wave.append(Interleaved::new(&[ramp_value(i)], wave.layout()).unwrap())
            .unwrap();
    }
    let clip = Clip::new(wave)
        .and_then(|c| c.channels(ChannelLayout::STEREO))
        .expect("a placeable wave");
    let (key, handle) = s.graph().add(clip);
    wire(s, key.erase());
    let _ = s.update();
    (key, handle)
}

/// The first output channel of `frames` frames.
fn left(s: &Session, frames: usize) -> Vec<f32> {
    render(s, frames).chunks(2).map(|f| f[0]).collect()
}

/// **`play` and `stop` refuse a placed voice and drive a free-running one,
/// by its own cursor**: a placed ramp clip plays source frame `f` at frame
/// `f` and is `Placed` for both verbs, until its window is `None` and
/// published. Then the voice is free: `play` sounds the ramp from its frame 0
/// (not from where the transport is: the placed to free transition), `stop`
/// silences it, `play` resumes without rewinding, and it is silent from the
/// ramp's end on.
#[test]
fn play_on_a_placed_voice_is_refused_and_a_free_running_one_plays_its_own_cursor() {
    let mut s = offline();
    let (clip, handle) = placed_ramp(&mut s);
    s.play().expect("room");
    let placed: Vec<f32> = left(&s, 1_024);
    assert!(placed.iter().enumerate().all(|(f, &x)| x == ramp_value(f)));
    assert_eq!(handle.play(), Err(PlayError::Placed));
    assert_eq!(handle.stop(), Err(PlayError::Placed));

    s.set(clip.window().to(None)).unwrap();
    assert_eq!(handle.play(), Err(PlayError::Placed), "until the update");
    let _ = s.update();
    assert_eq!(handle.play(), Ok(()));
    let mut got = left(&s, 512);
    assert_eq!(handle.stop(), Ok(()));
    got.extend(left(&s, 512));
    assert_eq!(handle.play(), Ok(()));
    got.extend(left(&s, RAMP_LEN));

    for (f, &x) in got.iter().enumerate() {
        let want = match f {
            0..512 => ramp_value(f),
            512..1_024 => 0.0,
            _ if f - 512 < RAMP_LEN => ramp_value(f - 512),
            _ => 0.0,
        };
        assert_eq!(x.to_bits(), want.to_bits(), "frame {f}");
    }
}

/// **Two windows written before an update publish once, and the second wins**:
/// the cell's `seq` moves by one, and holds the second.
#[test]
fn two_window_writes_before_an_update_publish_once_and_the_second_wins() {
    let mut s = offline();
    let (clip, handle) = placed(&mut s, Beat(2.0));
    let before = handle.window_cell().seq();
    let second = Some(VoiceWindow::from(Beat(3.0)));
    s.set(clip.window().to(Some(VoiceWindow::from(Beat(1.0)))))
        .unwrap();
    s.set(clip.window().to(second)).unwrap();
    let _ = s.update();
    assert_eq!(handle.window_cell().seq(), before + 1);
    assert_eq!(*handle.window_cell().current(), second);
    let _ = s.update();
    assert_eq!(handle.window_cell().seq(), before + 1, "nothing pending");
}

/// **A window written to a node removed before the update never publishes**:
/// the node's cell keeps the window it had through the updates after, and the
/// target is gone.
#[test]
fn a_pending_window_write_to_a_removed_node_never_publishes() {
    let mut s = offline();
    let (clip, handle) = placed(&mut s, Beat(2.0));
    let (seq, in_force) = (handle.window_cell().seq(), handle.window_cell().current());
    s.set(clip.window().to(None)).unwrap();
    s.remove(clip).unwrap();
    let _ = s.update();
    let _ = s.update();
    assert_eq!(handle.window_cell().seq(), seq);
    assert_eq!(handle.window_cell().current(), in_force);
    let err = s.set(clip.window().to(None)).expect_err("the node is gone");
    assert_eq!(err.kind, ValueErrorKind::NoSuchTarget);
    assert_eq!(
        s.get(clip.window()).expect_err("gone").kind,
        ValueErrorKind::NoSuchTarget
    );
}

/// **A block that adopts a written window allocates nothing.**
#[test]
fn a_block_adopting_a_written_window_does_not_allocate() {
    let mut s = offline();
    let (clip, _) = placed(&mut s, Beat(2.0));
    s.play().expect("room");
    let _ = render(&s, 1_024);
    s.set(clip.window().to(Some(VoiceWindow::from(Beat(0.0)))))
        .unwrap();
    let _ = s.update();
    let mut buf = vec![0.0f32; 512 * 2];
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        }
    });
    assert!(buf.iter().any(|&x| x != 0.0), "the window took");
}

/// File frames of the ramp both tiers play. Frame `i` holds `i / 2^17`, exact
/// in `f32`, so an output sample names the source frame it was read from.
const LEN: usize = 8_000;

fn ramp(i: usize) -> f32 {
    i as f32 / 131_072.0
}

/// A clip of the ramp at beat 0 in `tier`, wired to the output, with the
/// session rolling and, for the streamed tier, the streamer that feeds it.
struct Rig {
    s: Session,
    clip: tutti_session::Key<Clip<f32>>,
    handle: tutti_sampler::VoiceNodeHandle<f32>,
    streamer: Option<tutti_sampler::DiskStreamer<tutti_sampler::ByHand<f32>, f32>>,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(streamed: bool) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let samples: Vec<f32> = (0..LEN).map(ramp).collect();
        let mut s = offline();
        let (clip, streamer) = if streamed {
            let path = dir.path().join("ramp.wav");
            tutti_fixtures::signal::write_wav_f32(&path, SampleRate::of(RATE), 1, &samples);
            let mut streamer =
                tutti_sampler::DiskStreamer::by_hand(SampleRate::of(RATE), Default::default())
                    .expect("builds");
            let opening = streamer.open(&path, tutti_sampler::Placement::HEAD);
            let (stream, reader) =
                tutti_sampler::testing::settle(&mut streamer, opening).expect("the file opens");
            (Clip::streaming(reader, stream), Some(streamer))
        } else {
            let mut wave = Wave::new(1, SampleRate::of(RATE));
            for &x in &samples {
                wave.append(Interleaved::new(&[x], wave.layout()).unwrap())
                    .unwrap();
            }
            (Clip::new(wave).expect("a placeable wave"), None)
        };
        let (clip, handle) = s.graph().add(clip.at(Beat(0.0)));
        for port in 0..2 {
            s.graph()
                .set_sources_hard(Root::output(port), [OutPort::new(clip.erase(), 0)])
                .unwrap();
        }
        let _ = s.update();
        s.play().expect("room");
        let mut rig = Self {
            s,
            clip,
            handle,
            streamer,
            _dir: dir,
        };
        rig.settle();
        rig
    }

    /// The streamer's work done: the ring holds what its cells ask for.
    fn settle(&mut self) {
        if let Some(streamer) = &mut self.streamer {
            let _ = streamer.step_until_settled(1_000);
        }
    }

    /// `frames` frames of the first output, in 512-frame blocks, the streamer
    /// stepped between them as a running one is.
    fn render(&mut self, frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; 512 * 2];
        while out.len() < frames {
            self.s
                .render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
            out.extend(buf.chunks(2).map(|f| f[0]));
            self.settle();
        }
        out.truncate(frames);
        out
    }
}

fn loop_1000_3000() -> Option<tutti_sampler::Loop> {
    Some(tutti_sampler::testing::loop_frames(1_000, 3_000, 0))
}

/// **A loop set is pending until the update, and then the voice wraps at its
/// end**, in either tier: a loop over frames `1000..3000` is written. A read
/// answers it while the node's cell holds none; after the update the cell
/// holds it, and the output is the source frame `f` up to the loop's end, the
/// position wrapping to `1000` on frame 3000 and every 2000 frames after
/// (source frame `1000 + (f - 3000) % 2000`). Resident: the node applies it.
/// Streamed: the streamer reads the same cell and republishes the ring's
/// layout.
fn a_loop_set_wraps_the_voice_at_its_end(streamed: bool) {
    let mut rig = Rig::new(streamed);
    let looped = loop_1000_3000();
    rig.s.set(rig.clip.looping().to(looped)).unwrap();
    assert_eq!(rig.s.get(rig.clip.looping()).unwrap(), looped);
    assert_eq!(*rig.handle.looping_cell().current(), None, "pending");
    let _ = rig.s.update();
    assert_eq!(*rig.handle.looping_cell().current(), looped);
    rig.settle();
    let out = rig.render(7_000);
    for (f, &got) in out.iter().enumerate() {
        let source = if f < 3_000 {
            f
        } else {
            1_000 + (f - 3_000) % 2_000
        };
        assert_eq!(
            got.to_bits(),
            ramp(source).to_bits(),
            "frame {f}: source {source}"
        );
    }
}

#[test]
fn a_loop_set_lands_at_the_update_and_wraps_a_resident_voice_at_its_end() {
    a_loop_set_wraps_the_voice_at_its_end(false);
}

#[test]
fn a_loop_set_lands_at_the_update_and_wraps_a_streamed_voice_at_its_end() {
    a_loop_set_wraps_the_voice_at_its_end(true);
}

/// **A direction set is pending until the update, and then the voice plays
/// the source backwards from its last frame**, in either tier: output frame
/// `f` is source frame `LEN - 1 - f` (a streamed voice from its second block).
fn a_direction_set_plays_the_voice_backwards(streamed: bool) {
    let mut rig = Rig::new(streamed);
    rig.s
        .set(rig.clip.direction().to(tutti_sampler::Direction::Reverse))
        .unwrap();
    assert_eq!(
        rig.s.get(rig.clip.direction()).unwrap(),
        tutti_sampler::Direction::Reverse
    );
    assert_eq!(
        *rig.handle.direction_cell().current(),
        tutti_sampler::Direction::Forward,
        "pending"
    );
    let _ = rig.s.update();
    rig.settle();
    let out = rig.render(LEN);
    // The streamer turns its ring from a switch point a guard past the block
    // the reader is in, crossfaded, so a streamed voice plays backwards from
    // the second block on; a resident one from frame 0.
    let from = if streamed { 1_024 } else { 0 };
    for (f, &got) in out.iter().enumerate().skip(from) {
        assert_eq!(
            got.to_bits(),
            ramp(LEN - 1 - f).to_bits(),
            "frame {f}: source {}",
            LEN - 1 - f
        );
    }
}

#[test]
fn a_direction_set_lands_at_the_update_and_plays_a_resident_voice_backwards() {
    a_direction_set_plays_the_voice_backwards(false);
}

#[test]
fn a_direction_set_lands_at_the_update_and_plays_a_streamed_voice_backwards() {
    a_direction_set_plays_the_voice_backwards(true);
}

/// **Two loops or directions written before an update publish once, and the
/// second wins**: each cell's `seq` moves by one.
#[test]
fn two_loop_and_direction_writes_before_an_update_publish_once_and_the_second_wins() {
    let mut rig = Rig::new(false);
    let cells = (
        rig.handle.looping_cell().clone(),
        rig.handle.direction_cell().clone(),
    );
    let before = (cells.0.seq(), cells.1.seq());
    let second = Some(tutti_sampler::testing::loop_frames(500, 900, 0));
    for looped in [loop_1000_3000(), second] {
        rig.s.set(rig.clip.looping().to(looped)).unwrap();
    }
    for direction in [
        tutti_sampler::Direction::Reverse,
        tutti_sampler::Direction::Forward,
    ] {
        rig.s.set(rig.clip.direction().to(direction)).unwrap();
    }
    let _ = rig.s.update();
    assert_eq!(*cells.0.current(), second);
    assert_eq!(cells.0.seq(), before.0 + 1);
    assert_eq!(
        cells.1.seq(),
        before.1,
        "a direction written back to the one in force publishes nothing"
    );
    let _ = rig.s.update();
    assert_eq!(cells.0.seq(), before.0 + 1, "nothing pending");
}

/// **A loop or direction written to a node removed before the update never
/// publishes**: the node's cells keep what they held through the updates
/// after, and the targets are gone.
#[test]
fn a_pending_loop_or_direction_write_to_a_removed_node_never_publishes() {
    let mut rig = Rig::new(false);
    let cells = (
        rig.handle.looping_cell().clone(),
        rig.handle.direction_cell().clone(),
    );
    let seqs = (cells.0.seq(), cells.1.seq());
    rig.s.set(rig.clip.looping().to(loop_1000_3000())).unwrap();
    rig.s
        .set(rig.clip.direction().to(tutti_sampler::Direction::Reverse))
        .unwrap();
    let clip = rig.clip;
    rig.s.remove(clip).unwrap();
    let _ = rig.s.update();
    let _ = rig.s.update();
    assert_eq!((cells.0.seq(), cells.1.seq()), seqs);
    assert_eq!(*cells.0.current(), None);
    assert_eq!(*cells.1.current(), tutti_sampler::Direction::Forward);
    let err = rig
        .s
        .set(clip.looping().to(None))
        .expect_err("the node is gone");
    assert_eq!(err.kind, ValueErrorKind::NoSuchTarget);
    assert_eq!(
        rig.s.get(clip.direction()).expect_err("gone").kind,
        ValueErrorKind::NoSuchTarget
    );
}

/// **Blocks that adopt a written loop and direction allocate nothing**, in
/// either tier (the streamer's work on the streamed one is done outside).
fn blocks_adopting_a_loop_and_direction_do_not_allocate(streamed: bool) {
    let mut rig = Rig::new(streamed);
    let _ = rig.render(1_024);
    rig.s.set(rig.clip.looping().to(loop_1000_3000())).unwrap();
    rig.s
        .set(rig.clip.direction().to(tutti_sampler::Direction::Reverse))
        .unwrap();
    let _ = rig.s.update();
    rig.settle();
    let mut buf = vec![0.0f32; 512 * 2];
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            rig.s
                .render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        }
    });
    // The direction was adopted: the last block's first frame is frame 2 560
    // of the voice, which plays the source backwards.
    assert_eq!(buf[0].to_bits(), ramp(LEN - 1 - 2_560).to_bits());
}

#[test]
fn blocks_adopting_a_loop_and_direction_do_not_allocate_on_a_resident_voice() {
    blocks_adopting_a_loop_and_direction_do_not_allocate(false);
}

#[test]
fn blocks_adopting_a_loop_and_direction_do_not_allocate_on_a_streamed_voice() {
    blocks_adopting_a_loop_and_direction_do_not_allocate(true);
}
