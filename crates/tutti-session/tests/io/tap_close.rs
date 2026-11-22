//! A tap opened and closed on a running detached session.
//!
//! `engine.rs` covers a tap fed by a render
//! (`a_detached_render_feeds_a_tap`, `an_offline_render_feeds_an_output_tap`)
//! and one closed by its reader's drop
//! (`a_dropped_reader_leaves_the_plan_at_the_reclaim_phase`); what it does not
//! is a tap opened after the first render and one closed by name
//! (`Graph::close_tap`).

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{CloseError, DetachedSession, MainOutput, OutputPort, Session};
use tutti_units::{SampleRate, Samples};

/// A detached one-output session playing a constant 0.25.
fn session() -> DetachedSession {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let dc = s.graph().add(ForkByClone(Const::mono(0.25)));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(dc, 0)])
        .unwrap()
        .unfaded();
    s
}

/// The whole point of the seam: a host gets the reader from the graph, and it
/// sees what the session rendered since the open.
#[test]
fn opening_a_tap_yields_a_reader_that_sees_the_rendered_frames() {
    let mut s = session();
    let mut out = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    let mut reader = s
        .graph()
        .taps()
        .open_main_output(Samples::new(64))
        .expect("the sources exist");
    for _ in 0..3 {
        s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    }

    let mut got = [f32::NAN; 8];
    assert_eq!(
        reader
            .pop_into(InterleavedMut::new(&mut got, reader.layout()).unwrap())
            .unwrap(),
        Samples::new(3),
        "three frames rendered since the open"
    );
    assert_eq!(got[..3], [0.25; 3]);
}

/// Closing stops the copy, so a host that finishes analysing is back to a plan
/// with no tap op.
#[test]
fn closing_the_tap_stops_the_copy() {
    let mut s = session();
    let mut reader = s
        .graph()
        .taps()
        .open_main_output(Samples::new(64))
        .expect("the sources exist");
    let mut out = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    let id = reader.id().expect("the session named it");
    s.graph().taps().close(id).unwrap();
    assert_eq!(
        s.graph().taps().close(id),
        Err(CloseError::Tap(id)),
        "a closed tap is not open"
    );
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());

    let mut got = [f32::NAN; 8];
    assert_eq!(
        reader
            .pop_into(InterleavedMut::new(&mut got, reader.layout()).unwrap())
            .unwrap(),
        Samples::new(1),
        "only the frame rendered while open"
    );
}

/// A session that never opened a tap renders as it always did, and the reader
/// a later open returns starts empty: a tap flows from its commit.
#[test]
fn a_session_without_a_tap_renders_untouched() {
    let mut s = session();
    let mut out = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(out[0], 0.25);
    let reader = s
        .graph()
        .taps()
        .open_main_output(Samples::new(64))
        .expect("the sources exist");
    assert_eq!(
        reader.available(),
        Samples::new(0),
        "a tap flows from its commit"
    );
}

/// A tap the session opens carries a bell: its `notified` is pending until a
/// block renders, ready after one, and pending again once it has resolved with
/// nothing more rendered.
#[test]
fn a_session_taps_notified_resolves_once_a_block_renders() {
    let mut s = session();
    let mut reader = s
        .graph()
        .taps()
        .open_main_output(Samples::new(64))
        .expect("the sources exist");
    let mut cx = Context::from_waker(Waker::noop());
    let mut poll = |reader: &mut tutti_graph::TapReader| {
        pin!(reader.notified().expect("a session's tap carries a bell")).poll(&mut cx)
    };
    assert_eq!(poll(&mut reader), Poll::Pending, "nothing rendered yet");
    let mut out = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    assert_eq!(poll(&mut reader), Poll::Ready(()), "a block was written");
    assert_eq!(poll(&mut reader), Poll::Pending, "the ring was consumed");
}
