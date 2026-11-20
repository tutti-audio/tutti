//! `TransportHandle`: the transport's verbs from another thread.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_music::LoopRange;
use tutti_session::{Session, TransportHandle};
use tutti_timeline::{FadeOut, LoopRegion, MotionCommand, TimedError};
use tutti_units::{At, Beat, Bpm, Frame, Samples};

fn offline() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(1_000)))
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
    let mut s = offline();
    crate::tempo::set_tempo(&mut s, Bpm::of(60.0)).expect("not following");
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
    let refused = std::thread::spawn(move || handle.schedule(Beat::new(1.0), MotionCommand::Play))
        .join()
        .unwrap();
    assert!(matches!(
        refused,
        Err(TimedError::BeatWhileStopped(MotionCommand::Play))
    ));
    assert!(matches!(
        s.schedule(Beat::new(1.0), MotionCommand::Play),
        Err(TimedError::BeatWhileStopped(MotionCommand::Play))
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
    handle
        .schedule(Beat::new(4.0), MotionCommand::Play)
        .unwrap();
    handle.stop(FadeOut::Immediate).unwrap();
    assert!(matches!(
        s.schedule(Beat::new(4.0), MotionCommand::Play),
        Err(TimedError::BeatWhileStopped(_))
    ));
}

/// **A play scheduled for the next block is a play the record knows**: a beat
/// play after it is accepted, and a next-block stop makes the same beat play
/// refused.
#[test]
fn a_next_block_schedule_updates_the_rolling_record() {
    let s = offline();
    let handle = s.transport_handle();
    handle.schedule(At::NextBlock, MotionCommand::Play).unwrap();
    assert!(handle.schedule(Beat::new(4.0), MotionCommand::Play).is_ok());
    handle
        .schedule(At::NextBlock, MotionCommand::stop_unfaded())
        .unwrap();
    assert!(matches!(
        handle.schedule(Beat::new(5.0), MotionCommand::Play),
        Err(TimedError::BeatWhileStopped(MotionCommand::Play))
    ));
}

/// **A stop sent after a rendered play refuses a beat play before the stop
/// renders:** the record speaks until a render passes it, so the rolling
/// playhead of the block before does not let a play wait on a stopped
/// transport.
#[test]
fn a_stop_sent_after_a_rendered_play_refuses_a_beat_play() {
    let s = offline();
    s.play().unwrap();
    render(&s, 2);
    assert!(s.playhead().playing());
    let handle = s.transport_handle();
    handle.stop(FadeOut::Immediate).unwrap();
    assert!(matches!(
        handle.schedule(Beat::new(8.0), MotionCommand::Play),
        Err(TimedError::BeatWhileStopped(MotionCommand::Play))
    ));
}

/// **A play timed at a frame rolls the transport for the record once it
/// fires:** after the render that fires it, a beat play is accepted.
#[test]
fn a_play_timed_at_a_frame_counts_as_rolling_once_rendered() {
    let s = offline();
    let handle = s.transport_handle();
    handle
        .schedule(Frame::new(1_000), MotionCommand::Play)
        .unwrap();
    assert!(matches!(
        handle.schedule(Beat::new(8.0), MotionCommand::Play),
        Err(TimedError::BeatWhileStopped(MotionCommand::Play))
    ));
    render(&s, 3);
    assert!(s.playhead().playing());
    assert!(handle.schedule(Beat::new(8.0), MotionCommand::Play).is_ok());
}

/// **A handle reads and cancels the timed commands in flight.**
#[test]
fn a_handle_counts_and_cancels_scheduled_commands() {
    let s = offline();
    let handle = s.transport_handle();
    handle
        .schedule(Frame::new(48_000), MotionCommand::Play)
        .unwrap();
    assert_eq!(handle.scheduled_outstanding(), 1);
    handle.cancel_scheduled();
    render(&s, 1);
    assert_eq!(handle.scheduled_outstanding(), 0);
    assert!(!s.playhead().playing(), "the cancelled play never fires");
}

/// **A handle reads the loop region the session sets and disarms.**
#[test]
fn a_handle_reads_the_loop_region() {
    let mut s = offline();
    let handle = s.transport_handle();
    let range = LoopRange::try_from(Beat::new(0.0)..Beat::new(1.0)).unwrap();
    let looping = handle.looping();
    s.set(looping.to(LoopRegion::new(range, true))).unwrap();
    let _published = s.update();
    render(&s, 1);
    let region = handle.loop_region();
    assert_eq!(region.bounds(), range);
    assert!(region.is_armed());
    s.set(looping.to(LoopRegion::new(range, false))).unwrap();
    let _published = s.update();
    render(&s, 1);
    let region = handle.loop_region();
    assert_eq!(region.bounds(), range, "the bounds are kept");
    assert!(!region.is_armed());
}
