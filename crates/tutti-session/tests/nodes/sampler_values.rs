//! A sampler voice's steering values written with the session's verbs: its
//! window (`clip.window()`) is staged at `set` and published by the next
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
