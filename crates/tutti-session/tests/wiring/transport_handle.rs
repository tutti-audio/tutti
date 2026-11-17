//! `TransportHandle`: the transport's verbs from another thread.

use tutti_buffers::InterleavedMut;
use tutti_session::{Session, TransportHandle};
use tutti_transport::{FadeOut, MotionEvent, TimedError};
use tutti_types::{Beat, Bpm, ChannelLayout, MaxBlock, Samples};

fn offline() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples(1_000)))
        .build()
        .expect("builds")
}

fn render(s: &Session, blocks: usize) {
    let mut block = vec![0.0f32; 1_000 * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    }
}

const fn assert_send_clone<T: Send + Clone>() {}
const _: () = assert_send_clone::<TransportHandle>();

/// **A handle moved to another thread plays the transport**: the session's
/// next block reads rolling and the playhead has moved past the origin.
#[test]
fn a_handle_on_another_thread_rolls_the_session_transport() {
    let s = offline();
    s.set_tempo(Bpm::of(60.0)).expect("not following");
    let handle = s.transport_handle();
    std::thread::spawn(move || handle.play().unwrap())
        .join()
        .unwrap();
    render(&s, 2);
    let head = s.playhead();
    assert!(head.playing());
    assert!(head.beat().get() > 0.0, "{}", head.beat().get());
}

/// **A handle's stop reaches the session**: after a play from the session and
/// a stop from a thread, the playhead holds.
#[test]
fn a_handle_stop_halts_a_session_play() {
    let s = offline();
    s.play().unwrap();
    render(&s, 2);
    let handle = s.transport_handle();
    std::thread::spawn(move || handle.stop(FadeOut::Immediate).unwrap())
        .join()
        .unwrap();
    render(&s, 2);
    let held = s.playhead().beat();
    render(&s, 5);
    assert!(!s.playhead().playing());
    assert_eq!(s.playhead().beat(), held);
}

/// **A beat-stamped play from a handle while stopped is refused off-thread**
/// as the session refuses it, and the refusal hands the event back.
#[test]
fn a_handle_refuses_a_beat_play_while_stopped() {
    let s = offline();
    let handle = s.transport_handle();
    let refused = std::thread::spawn(move || handle.schedule(Beat(1.0), MotionEvent::Play))
        .join()
        .unwrap();
    assert!(matches!(
        refused,
        Err(TimedError::BeatWhileStopped(MotionEvent::Play))
    ));
    assert!(matches!(
        s.schedule(Beat(1.0), MotionEvent::Play),
        Err(TimedError::BeatWhileStopped(MotionEvent::Play))
    ));
}

/// **The rolling record is shared**: a play sent through the session lets a
/// handle schedule a beat-stamped play, and a handle's stop makes the session
/// refuse one.
#[test]
fn the_session_and_its_handles_share_the_rolling_record() {
    let s = offline();
    let handle = s.transport_handle();
    s.play().unwrap();
    handle.schedule(Beat(4.0), MotionEvent::Play).unwrap();
    handle.stop(FadeOut::Immediate).unwrap();
    assert!(matches!(
        s.schedule(Beat(4.0), MotionEvent::Play),
        Err(TimedError::BeatWhileStopped(_))
    ));
}
