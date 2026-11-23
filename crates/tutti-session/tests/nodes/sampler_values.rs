//! A sampler voice's steering values written with the session's verbs: its
//! window (`clip.window()`), loop (`clip.looping()`), direction
//! (`clip.direction()`) and speed (`clip.speed()`) are staged at `set` and
//! published by the next `update()`.

use crate::offline::offline_48k;
use tutti_buffers::{ChannelLayout, Interleaved, InterleavedMut, Wave};
use tutti_graph::NodeKey;
use tutti_sampler::{Clip, PlayError, VoiceWindow};
use tutti_session::{MainOutput, OutputPort, Session, ValueErrorKind};
use tutti_units::{Amplitude, Beat, SampleRate};

const RATE: f64 = 48_000.0;

/// Frames a beat takes at the session's 120 BPM and 48 kHz.
const BEAT: usize = 24_000;

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
fn render(s: &mut Session, frames: usize) -> Vec<f32> {
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
            .set_sources(MainOutput::channel(port), [OutputPort::new(node, port)])
            .unwrap()
            .unfaded();
    }
}

/// The graph's handle of `clip`, cloned: the cells and verbs a test reads
/// beside the key's targets.
fn handle_of(
    s: &Session,
    clip: &tutti_session::Key<Clip<f32>>,
) -> tutti_sampler::VoiceNodeHandle<f32> {
    s.graph_ref()
        .handle(clip)
        .expect("the clip is in the graph")
        .clone()
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
        .gain(Amplitude::new(1.0))
        .expect("a finite gain");
    let key = s.graph().add(clip);
    let handle = handle_of(s, &key);
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
    let mut s = offline_48k();
    let (clip, handle) = placed(&mut s, Beat::new(2.0));
    let beat_one = Some(VoiceWindow::from(Beat::new(1.0)));

    s.set(clip.window().to(beat_one)).unwrap();
    assert_eq!(
        s.get(clip.window()).unwrap(),
        beat_one,
        "the pending window"
    );
    assert_eq!(
        *handle.window_cell().current(),
        Some(VoiceWindow::from(Beat::new(2.0))),
        "the node's cell holds the window in force"
    );
    s.play().expect("room");
    assert_eq!(
        first_sound(&render(&mut s, 40_000)),
        None,
        "nothing before the update"
    );

    let mut s = offline_48k();
    let (clip, handle) = placed(&mut s, Beat::new(2.0));
    s.set(clip.window().to(beat_one)).unwrap();
    let _ = s.update();
    assert_eq!(*handle.window_cell().current(), beat_one);
    s.play().expect("room");
    assert_eq!(first_sound(&render(&mut s, 40_000)), Some(BEAT));
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
    let key = s.graph().add(clip);
    let handle = handle_of(s, &key);
    wire(s, key.erase());
    let _ = s.update();
    (key, handle)
}

/// The first output channel of `frames` frames.
fn left(s: &mut Session, frames: usize) -> Vec<f32> {
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
    let mut s = offline_48k();
    let (clip, handle) = placed_ramp(&mut s);
    s.play().expect("room");
    let placed: Vec<f32> = left(&mut s, 1_024);
    assert!(placed.iter().enumerate().all(|(f, &x)| x == ramp_value(f)));
    assert_eq!(handle.play(), Err(PlayError::Placed));
    assert_eq!(handle.stop(), Err(PlayError::Placed));

    s.set(clip.window().to(None)).unwrap();
    assert_eq!(handle.play(), Err(PlayError::Placed), "until the update");
    let _ = s.update();
    assert_eq!(handle.play(), Ok(()));
    let mut got = left(&mut s, 512);
    assert_eq!(handle.stop(), Ok(()));
    got.extend(left(&mut s, 512));
    assert_eq!(handle.play(), Ok(()));
    got.extend(left(&mut s, RAMP_LEN));

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
    let mut s = offline_48k();
    let (clip, handle) = placed(&mut s, Beat::new(2.0));
    let before = handle.window_cell().seq();
    let second = Some(VoiceWindow::from(Beat::new(3.0)));
    s.set(clip.window().to(Some(VoiceWindow::from(Beat::new(1.0)))))
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
    let mut s = offline_48k();
    let (clip, handle) = placed(&mut s, Beat::new(2.0));
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
    let mut s = offline_48k();
    let (clip, _) = placed(&mut s, Beat::new(2.0));
    s.play().expect("room");
    let _ = render(&mut s, 1_024);
    s.set(clip.window().to(Some(VoiceWindow::from(Beat::new(0.0)))))
        .unwrap();
    let _ = s.update();
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..4 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
    assert!(buf.iter().any(|&x| x != 0.0), "the window took");
}

/// File frames of the ramp both tiers play. Frame `i` holds `i / 2^17`, exact
/// in `f32`, so an output sample names the source frame it was read from.
const LEN: usize = 8_000;

/// Frames of a device block the rig renders in.
const BLOCK: usize = 512;

fn ramp(i: usize) -> f32 {
    i as f32 / 131_072.0
}

/// A clip of the ramp at beat 0 in `tier`, wired to the output, with the
/// session rolling and, for the streamed tier, the streamer that feeds it.
struct Rig {
    s: Session,
    clip: tutti_session::Key<Clip<f32>>,
    handle: tutti_sampler::VoiceNodeHandle<f32>,
    streamer: Option<tutti_sampler::DiskStreamer<tutti_sampler::Manual<f32>, f32>>,
    /// Frames rendered so far: the frame the next render starts at.
    at: usize,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(streamed: bool) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let samples: Vec<f32> = (0..LEN).map(ramp).collect();
        let mut s = offline_48k();
        let (clip, streamer) = if streamed {
            let path = dir.path().join("ramp.wav");
            tutti_fixtures::signal::write_wav_f32(&path, SampleRate::of(RATE), 1, &samples);
            let mut streamer =
                tutti_sampler::DiskStreamer::manual(SampleRate::of(RATE), Default::default())
                    .expect("builds");
            let opening = streamer.open_stream(&path, tutti_sampler::StreamStart::HEAD);
            let (stream, reader) = tutti_sampler::testing::wait_for_open(&mut streamer, opening)
                .expect("the file opens");
            (Clip::streaming(reader, stream), Some(streamer))
        } else {
            let mut wave = Wave::new(1, SampleRate::of(RATE));
            for &x in &samples {
                wave.append(Interleaved::new(&[x], wave.layout()).unwrap())
                    .unwrap();
            }
            (Clip::new(wave).expect("a placeable wave"), None)
        };
        let clip = s.graph().add(clip.at(Beat::new(0.0)));
        let handle = handle_of(&s, &clip);
        for port in 0..2 {
            s.graph()
                .set_sources(
                    MainOutput::channel(port),
                    [OutputPort::new(clip.erase(), 0)],
                )
                .unwrap()
                .unfaded();
        }
        let _ = s.update();
        s.play().expect("room");
        let mut rig = Self {
            s,
            clip,
            handle,
            streamer,
            at: 0,
            _dir: dir,
        };
        rig.settle();
        rig
    }

    /// The streamer's work done: the ring holds what its cells ask for.
    fn settle(&mut self) {
        if let Some(streamer) = &mut self.streamer {
            let _ = streamer.step_until_idle(1_000);
        }
    }

    /// `frames` frames of the first output, in 512-frame blocks, the streamer
    /// stepped between them as a running one is.
    fn render(&mut self, frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; BLOCK * 2];
        while out.len() < frames {
            self.s
                .render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
            out.extend(buf.chunks(2).map(|f| f[0]));
            self.settle();
        }
        out.truncate(frames);
        self.at += frames;
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
        .set(
            rig.clip
                .direction()
                .to(tutti_sampler::PlayDirection::Reverse),
        )
        .unwrap();
    assert_eq!(
        rig.s.get(rig.clip.direction()).unwrap(),
        tutti_sampler::PlayDirection::Reverse
    );
    assert_eq!(
        *rig.handle.direction_cell().current(),
        tutti_sampler::PlayDirection::Forward,
        "pending"
    );
    let _ = rig.s.update();
    rig.settle();
    let out = rig.render(LEN);
    if streamed {
        // The streamer turns its ring from a switch point a guard past the
        // block the reader is in, crossfaded over the seek crossfade: the
        // voice plays forwards up to the guard, blends the two across the
        // crossfade (each frame between the forward and the reversed one),
        // and plays backwards from the block, the guard and the crossfade on.
        let guard = tutti_sampler::GUARD_FRAMES.get();
        let fade = tutti_sampler::DiskStreamerConfig::default()
            .buffer_config
            .seek_crossfade_frames
            .get();
        for (f, &got) in out.iter().enumerate().take(guard) {
            assert_eq!(got.to_bits(), ramp(f).to_bits(), "frame {f} plays forwards");
        }
        for (f, &got) in out.iter().enumerate().skip(guard).take(fade) {
            let (a, b) = (ramp(f), ramp(LEN - 1 - f));
            let (lo, hi) = (a.min(b), a.max(b));
            assert!(
                (lo..=hi).contains(&got),
                "frame {f}: {got} is not between {lo} and {hi}"
            );
        }
        let landed = out
            .iter()
            .enumerate()
            .rposition(|(f, &x)| x.to_bits() != ramp(LEN - 1 - f).to_bits())
            .map_or(0, |last| last + 1);
        assert!(
            landed <= BLOCK + guard + fade,
            "the turn landed at frame {landed}, past a block, the guard and the crossfade"
        );
        assert!(landed > guard, "the turn is not instant: it crossfades");
        for (f, &got) in out.iter().enumerate().skip(BLOCK + guard + fade) {
            assert_eq!(got.to_bits(), ramp(LEN - 1 - f).to_bits(), "frame {f}");
        }
    } else {
        for (f, &got) in out.iter().enumerate() {
            assert_eq!(got.to_bits(), ramp(LEN - 1 - f).to_bits(), "frame {f}");
        }
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
        tutti_sampler::PlayDirection::Reverse,
        tutti_sampler::PlayDirection::Forward,
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
        .set(
            rig.clip
                .direction()
                .to(tutti_sampler::PlayDirection::Reverse),
        )
        .unwrap();
    let clip = rig.clip;
    rig.s.remove(clip).unwrap();
    let _ = rig.s.update();
    let _ = rig.s.update();
    assert_eq!((cells.0.seq(), cells.1.seq()), seqs);
    assert_eq!(*cells.0.current(), None);
    assert_eq!(*cells.1.current(), tutti_sampler::PlayDirection::Forward);
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
        .set(
            rig.clip
                .direction()
                .to(tutti_sampler::PlayDirection::Reverse),
        )
        .unwrap();
    let _ = rig.s.update();
    rig.settle();
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..4 {
        rig.s
            .render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
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

/// Frames after an update by which a change is fully heard: at once on a
/// resident voice; on a streamed one a block, the guard and the crossfade on
/// (and a few frames for the fade's tail).
fn landing(streamed: bool) -> usize {
    if !streamed {
        return 0;
    }
    let fade = tutti_sampler::DiskStreamerConfig::default()
        .buffer_config
        .seek_crossfade_frames
        .get();
    BLOCK + tutti_sampler::GUARD_FRAMES.get() + fade + 8
}

/// Asserts `out`, rendered from frame `start`, is `source(f)` at every frame
/// `f` from `from` on, bit for bit.
fn assert_source(out: &[f32], start: usize, from: usize, source: impl Fn(usize) -> usize) {
    for (i, &got) in out.iter().enumerate() {
        let f = start + i;
        if f >= from {
            assert_eq!(
                got.to_bits(),
                ramp(source(f)).to_bits(),
                "frame {f}: source {}",
                source(f)
            );
        }
    }
}

/// **A loop set mid-play wraps the voice at its end**: the voice plays
/// forwards over 1 024 frames, then a loop over `1000..3000` is written; the
/// voice plays on to frame 3 000 and wraps there and every 2 000 frames after.
fn a_loop_set_mid_play_wraps_at_its_end(streamed: bool) {
    let mut rig = Rig::new(streamed);
    let before = rig.render(1_024);
    assert_source(&before, 0, 0, |f| f);
    rig.s.set(rig.clip.looping().to(loop_1000_3000())).unwrap();
    let _ = rig.s.update();
    let after = rig.render(6_000);
    let wrap = |f: usize| {
        if f < 3_000 {
            f
        } else {
            1_000 + (f - 3_000) % 2_000
        }
    };
    assert_source(&after, 1_024, 1_024 + landing(streamed), wrap);
}

/// **A loop cleared mid-play plays the file straight on**: inside the second
/// lap of a loop over `1000..3000`, the loop is cleared; the voice, at
/// straight position `f`, plays file frame `f` from then on.
fn a_loop_cleared_mid_play_plays_straight_on(streamed: bool) {
    let mut rig = Rig::new(streamed);
    rig.s.set(rig.clip.looping().to(loop_1000_3000())).unwrap();
    let _ = rig.s.update();
    rig.settle();
    let wrap = |f: usize| {
        if f < 3_000 {
            f
        } else {
            1_000 + (f - 3_000) % 2_000
        }
    };
    let before = rig.render(4_096);
    assert_source(&before, 0, 0, wrap);
    rig.s.set(rig.clip.looping().to(None)).unwrap();
    let _ = rig.s.update();
    let after = rig.render(3_072);
    assert_source(&after, 4_096, 4_096 + landing(streamed), |f| f);
}

/// **A direction reversed mid-play and turned forward again lands each time**:
/// forwards over 1 024 frames, reversed (frame `f` plays source `LEN - 1 - f`),
/// then forwards (frame `f` plays source `f`).
fn a_direction_reversed_then_forward_mid_play(streamed: bool) {
    use tutti_sampler::PlayDirection;
    let mut rig = Rig::new(streamed);
    let first = rig.render(1_024);
    assert_source(&first, 0, 0, |f| f);
    rig.s
        .set(rig.clip.direction().to(PlayDirection::Reverse))
        .unwrap();
    let _ = rig.s.update();
    let reversed = rig.render(3_072);
    assert_source(&reversed, 1_024, 1_024 + landing(streamed), |f| LEN - 1 - f);
    rig.s
        .set(rig.clip.direction().to(PlayDirection::Forward))
        .unwrap();
    let _ = rig.s.update();
    let forward = rig.render(3_072);
    assert_source(&forward, 4_096, 4_096 + landing(streamed), |f| f);
}

#[test]
fn a_loop_set_mid_play_wraps_a_resident_voice() {
    a_loop_set_mid_play_wraps_at_its_end(false);
}

#[test]
fn a_loop_set_mid_play_wraps_a_streamed_voice() {
    a_loop_set_mid_play_wraps_at_its_end(true);
}

#[test]
fn a_loop_cleared_mid_play_plays_a_resident_voice_straight_on() {
    a_loop_cleared_mid_play_plays_straight_on(false);
}

#[test]
fn a_loop_cleared_mid_play_plays_a_streamed_voice_straight_on() {
    a_loop_cleared_mid_play_plays_straight_on(true);
}

#[test]
fn a_direction_reversed_then_forward_mid_play_lands_on_a_resident_voice() {
    a_direction_reversed_then_forward_mid_play(false);
}

#[test]
fn a_direction_reversed_then_forward_mid_play_lands_on_a_streamed_voice() {
    a_direction_reversed_then_forward_mid_play(true);
}

/// **A direction turned mid-play on a free-running voice jumps to the mirrored
/// frame**: the free cursor is at `p` after `p` frames, and a reversed read
/// of cursor `p` is source frame `LEN - 1 - p`. After 1 024 frames of
/// `ramp(f)`, the next frame is `ramp(LEN - 1 - 1024)` and the voice goes on
/// backwards from there, bit for bit.
#[test]
fn a_direction_turned_mid_play_on_a_free_running_voice_jumps_to_the_mirrored_frame() {
    use tutti_sampler::PlayDirection;
    let mut rig = Rig::new(false);
    rig.s.set(rig.clip.window().to(None)).unwrap();
    let _ = rig.s.update();
    rig.handle.play().expect("a free-running voice");
    let forward = rig.render(1_024);
    assert_source(&forward, 0, 0, |f| f);
    rig.s
        .set(rig.clip.direction().to(PlayDirection::Reverse))
        .unwrap();
    let _ = rig.s.update();
    let reversed = rig.render(2_048);
    assert_source(&reversed, 1_024, 1_024, |f| LEN - 1 - f);
}

fn rate(x: f64) -> tutti_units::PlaybackRate {
    tutti_units::PlaybackRate::of(x)
}

/// **A speed set is pending until the update, and then the voice reads every
/// second source frame**, in either tier: speed 2 is written before any block.
/// A read answers it while the node's cell holds unity; after the update the
/// cell holds it, and output frame `f` is exactly source frame `2f`, from the
/// first block on (the streamer's ring is read by position, so the streamed
/// voice's node hands the rate on to the stream's playback cells and the
/// streamer fetches ahead at it between blocks).
fn a_speed_set_reads_every_second_frame(streamed: bool) {
    let mut rig = Rig::new(streamed);
    rig.s.set(rig.clip.speed().to(rate(2.0))).unwrap();
    assert_eq!(rig.s.get(rig.clip.speed()).unwrap(), rate(2.0));
    assert_eq!(
        *rig.handle.speed_cell().current(),
        tutti_units::PlaybackRate::UNITY,
        "pending"
    );
    let _ = rig.s.update();
    assert_eq!(*rig.handle.speed_cell().current(), rate(2.0));
    rig.settle();
    let out = rig.render(LEN / 2);
    for (f, &got) in out.iter().enumerate() {
        assert_eq!(got.to_bits(), ramp(2 * f).to_bits(), "frame {f}");
    }
}

#[test]
fn a_speed_set_lands_at_the_update_and_a_resident_voice_reads_every_second_frame() {
    a_speed_set_reads_every_second_frame(false);
}

#[test]
fn a_speed_set_lands_at_the_update_and_a_streamed_voice_reads_every_second_frame() {
    a_speed_set_reads_every_second_frame(true);
}

/// **Two speeds written before an update publish once, and the second wins**:
/// the cell's `seq` moves by one and holds the second; a speed written back to
/// the one in force publishes nothing.
#[test]
fn two_speed_writes_before_an_update_publish_once_and_the_second_wins() {
    let mut rig = Rig::new(false);
    let cell = rig.handle.speed_cell().clone();
    let before = cell.seq();
    rig.s.set(rig.clip.speed().to(rate(2.0))).unwrap();
    rig.s.set(rig.clip.speed().to(rate(0.5))).unwrap();
    let _ = rig.s.update();
    assert_eq!(*cell.current(), rate(0.5));
    assert_eq!(cell.seq(), before + 1);
    let _ = rig.s.update();
    assert_eq!(cell.seq(), before + 1, "nothing pending");
    rig.s.set(rig.clip.speed().to(rate(0.5))).unwrap();
    let _ = rig.s.update();
    assert_eq!(
        cell.seq(),
        before + 1,
        "the rate in force publishes nothing"
    );
}

/// **A speed written to a node removed before the update never publishes**:
/// the node's cell keeps the speed it had through the updates after, and the
/// target is gone.
#[test]
fn a_pending_speed_write_to_a_removed_node_never_publishes() {
    let mut rig = Rig::new(false);
    let cell = rig.handle.speed_cell().clone();
    let seq = cell.seq();
    rig.s.set(rig.clip.speed().to(rate(2.0))).unwrap();
    let clip = rig.clip;
    rig.s.remove(clip).unwrap();
    let _ = rig.s.update();
    let _ = rig.s.update();
    assert_eq!(cell.seq(), seq);
    assert_eq!(*cell.current(), tutti_units::PlaybackRate::UNITY);
    let err = rig
        .s
        .set(clip.speed().to(rate(2.0)))
        .expect_err("the node is gone");
    assert_eq!(err.kind, ValueErrorKind::NoSuchTarget);
    assert_eq!(
        rig.s.get(clip.speed()).expect_err("gone").kind,
        ValueErrorKind::NoSuchTarget
    );
}

/// **A block that adopts a written speed allocates nothing.**
#[test]
fn a_block_adopting_a_written_speed_does_not_allocate() {
    let mut rig = Rig::new(false);
    let _ = rig.render(1_024);
    rig.s.set(rig.clip.speed().to(rate(2.0))).unwrap();
    let _ = rig.s.update();
    let mut buf = vec![0.0f32; BLOCK * 2];
    for _ in 0..4 {
        rig.s
            .render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
    // The speed was adopted (the voice is placed, so a frame is read at the
    // transport's position times the speed): the last block's first frame is frame 2 560 of
    // the voice, which reads source frame 5 120.
    assert_eq!(buf[0].to_bits(), ramp(2 * 2_560).to_bits());
}

/// **A speed set mid-play on a placed voice reads every second frame from the
/// update on**, in either tier: the voice plays at unity for two blocks (so
/// output frame `f` is source frame `f`), then speed 2 is written and
/// published, and a placed voice reads the source at the transport's position
/// times the speed, so output frame `f` is source frame `2f`. A resident voice
/// takes it from the next block; a streamed one from [`landing`] on.
fn a_speed_set_mid_play_reads_every_second_frame(streamed: bool) {
    const CHANGE: usize = 2 * BLOCK;
    let mut rig = Rig::new(streamed);
    let before = rig.render(CHANGE);
    assert_source(&before, 0, 0, |f| f);
    rig.s.set(rig.clip.speed().to(rate(2.0))).unwrap();
    let _ = rig.s.update();
    rig.settle();
    let after = rig.render(LEN / 2 - CHANGE);
    let from = CHANGE + landing(streamed);
    assert_source(&after, CHANGE, from, |f| 2 * f);
    assert!(
        LEN / 2 - from > 1_000,
        "the check hears {} frames",
        LEN / 2 - from
    );
}

#[test]
fn a_speed_set_mid_play_reads_a_resident_placed_voice_at_twice_the_position() {
    a_speed_set_mid_play_reads_every_second_frame(false);
}

#[test]
fn a_speed_set_mid_play_reads_a_streamed_placed_voice_at_twice_the_position() {
    a_speed_set_mid_play_reads_every_second_frame(true);
}

/// **A speed set mid-play on a free-running voice continues from its cursor**:
/// the voice plays 512 frames at unity (output frame `k` is source frame `k`,
/// the cursor ending at 512), then speed 2 is published, and output frame
/// `512 + k` is source frame `512 + 2k`: two source frames per output frame
/// from the cursor, never a restart and never a jump.
#[test]
fn a_speed_set_mid_play_continues_a_free_running_voice_from_its_cursor() {
    let mut s = offline_48k();
    let (clip, handle) = placed_ramp(&mut s);
    s.set(clip.window().to(None)).unwrap();
    let _ = s.update();
    s.play().expect("room");
    assert_eq!(handle.play(), Ok(()));
    let mut got = left(&mut s, 512);
    s.set(clip.speed().to(rate(2.0))).unwrap();
    let _ = s.update();
    got.extend(left(&mut s, 1_024));
    for (f, &x) in got.iter().enumerate() {
        let source = if f < 512 { f } else { 512 + 2 * (f - 512) };
        assert_eq!(x.to_bits(), ramp_value(source).to_bits(), "frame {f}");
    }
}
