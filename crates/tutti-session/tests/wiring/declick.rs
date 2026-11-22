//! Declicked wiring (doc 030 section 4): the session graph's `connect` fades a
//! new edge in over `DECLICK`, and `disconnect`,
//! `set_sources` removals and the edges of a removed node fade out over the same
//! span, leaving the plan when the fade has ended. `Connection::unfaded` and
//! `Disconnection::unfaded` opt out.
//!
//! Every figure is analytic: a constant of 1.0 through an edge whose weight is
//! the smootherstep `x³(6x² − 15x + 10)` at `x = (j + 1) / 241` at fade frame
//! `j` (240 frames, `DECLICK` at 48 kHz), one frame per render call, so the
//! position of every sample is exact.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{ForkByClone, declick_frames};
use tutti_nodes::testing::Const;
use tutti_session::{DetachedSession, MainOutput, OutPort, Session};
use tutti_units::SampleRate;

/// `DECLICK` at the session's 48 kHz.
const SPAN: usize = 240;

/// The smootherstep of frame `j` of the fade.
fn smooth(j: usize) -> f64 {
    let x = (j + 1) as f64 / (SPAN + 1) as f64;
    x * x * x * (6.0 * x * x - 15.0 * x + 10.0)
}

fn session() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// `n` frames of output channel 0, one render call each.
fn frames(s: &mut DetachedSession, n: usize) -> Vec<f32> {
    (0..n)
        .map(|_| {
            let mut frame = [f32::NAN; 2];
            s.render_frame(InterleavedMut::one_frame(&mut frame).unwrap());
            frame[0]
        })
        .collect()
}

/// **A connect rises along the smootherstep, and the edge is at its weight
/// from `DECLICK` on.** The weight is `s((j + 1) / 241)` at fade frame `j`; the
/// update that follows the fade drops the entry, and the edge is then the plain
/// sum.
#[test]
fn a_connect_fades_in_and_reaches_unity_at_declick() {
    assert_eq!(declick_frames(SampleRate::SR_48K).get(), SPAN);
    let mut s = session();
    let src = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), MainOutput::channel(0))
        .unwrap();
    let _ = s.update();

    let got = frames(&mut s, SPAN + 64);
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        assert!(
            (f64::from(g) - smooth(j)).abs() < 1e-6,
            "frame {j}: {g}, smootherstep {}",
            smooth(j)
        );
    }
    assert!(
        got[SPAN..].iter().all(|&x| x == 1.0),
        "unity from the 241st"
    );

    // The ended fade is reported: the next update commits the plain edge.
    let sent = s.graph_ref().testing().commits_sent();
    let _ = s.update();
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        sent + 1,
        "the fade was settled"
    );
    assert!(frames(&mut s, 8).iter().all(|&x| x == 1.0));
    let _ = s.update();
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        sent + 1,
        "and only once"
    );
}

/// **`.unfaded()` is the old step**: full weight from the first frame, no entry,
/// no commit to settle.
#[test]
fn hard_connects_at_full_weight_at_once() {
    let mut s = session();
    let src = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert!(
        frames(&mut s, 8)
            .iter()
            .all(|&x| x.to_bits() == 1.0f32.to_bits())
    );
    let sent = s.graph_ref().testing().commits_sent();
    let _ = s.update();
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        sent,
        "nothing to settle"
    );
}

/// **A disconnect fades out: its last nonzero frame is inside the span**, the
/// edge is no source of its sink from the call on, and it leaves the plan after.
#[test]
fn a_disconnect_fades_out_inside_the_span_and_leaves_the_plan() {
    let mut s = session();
    let src = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert!(frames(&mut s, 4).iter().all(|&x| x == 1.0));

    assert!(
        s.graph()
            .disconnect(src.out(0), MainOutput::channel(0))
            .unwrap()
            .was_connected()
    );
    assert!(
        s.graph_ref()
            .sources(MainOutput::channel(0))
            .unwrap()
            .is_empty()
    );
    assert!(
        !s.graph()
            .disconnect(src.out(0), MainOutput::channel(0))
            .unwrap()
            .was_connected(),
        "already on its way out"
    );
    let _ = s.update();
    let got = frames(&mut s, SPAN + 64);
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        let want = 1.0 - smooth(j);
        assert!(
            (f64::from(g) - want).abs() < 1e-6,
            "frame {j}: {g}, smootherstep {want}"
        );
    }
    let last = got.iter().rposition(|&x| x != 0.0).expect("it was heard");
    assert!(
        last < SPAN,
        "the last nonzero frame {last} is inside the span"
    );

    let sent = s.graph_ref().testing().commits_sent();
    let _ = s.update();
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        sent + 1,
        "the edge left the plan"
    );
    assert!(frames(&mut s, 4).iter().all(|&x| x == 0.0));
}

/// **`Disconnection::unfaded` cuts at once**, and a connect that no plan has heard
/// leaves no fade behind.
#[test]
fn disconnect_hard_cuts_and_an_unheard_edge_just_goes() {
    let mut s = session();
    let src = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert!(frames(&mut s, 4).iter().all(|&x| x == 1.0));
    assert!(
        s.graph()
            .disconnect(src.out(0), MainOutput::channel(0))
            .unwrap()
            .unfaded()
            .was_connected()
    );
    let _ = s.update();
    assert!(frames(&mut s, 4).iter().all(|&x| x == 0.0), "no tail");

    // Connected and disconnected inside one edit: nothing was ever heard.
    s.graph()
        .connect(src.out(0), MainOutput::channel(0))
        .unwrap();
    assert!(
        s.graph()
            .disconnect(src.out(0), MainOutput::channel(0))
            .unwrap()
            .was_connected()
    );
    let _ = s.update();
    assert!(frames(&mut s, SPAN).iter().all(|&x| x == 0.0));
}

/// **A reconnect while the edge fades out turns it round** from the weight it
/// had, without a step.
#[test]
fn a_reconnect_during_a_fade_out_rises_from_where_it_was() {
    let mut s = session();
    let src = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    let _ = s.update();
    frames(&mut s, 2);
    s.graph()
        .disconnect(src.out(0), MainOutput::channel(0))
        .unwrap();
    let _ = s.update();
    let down = frames(&mut s, 128);
    s.graph()
        .connect(src.out(0), MainOutput::channel(0))
        .unwrap();
    let _ = s.update();
    let up = frames(&mut s, 8);
    assert!(
        (up[0] - *down.last().unwrap()).abs() < 1e-5,
        "no step at the turn: {} after {}",
        up[0],
        down.last().unwrap()
    );
    assert!(up[7] > up[0], "rising");
    assert_eq!(
        s.graph_ref().sources(MainOutput::channel(0)).unwrap(),
        [src.out(0)]
    );
}

/// **`set_sources` fades what it removes and what it adds**, and a removed
/// node's output fades out before the node goes.
#[test]
fn set_sources_and_remove_fade_what_leaves() {
    let mut s = session();
    let a = s.graph().add(ForkByClone(Const::mono(1.0)));
    let b = s.graph().add(ForkByClone(Const::mono(2.0)));
    let sink = MainOutput::channel(0);
    s.graph()
        .set_sources(sink, [OutPort::new(a.erase(), 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    frames(&mut s, 2);
    s.graph()
        .set_sources(sink, [OutPort::new(b.erase(), 0)])
        .unwrap();
    let _ = s.update();
    let got = frames(&mut s, SPAN);
    // a falls as 1 - s while b rises as 2 s: their sum at frame j.
    for (j, &g) in got.iter().enumerate() {
        let want = (1.0 - smooth(j)) + 2.0 * smooth(j);
        assert!(
            (f64::from(g) - want).abs() < 1e-6,
            "frame {j}: {g}, smootherstep sum {want}"
        );
    }
    s.remove(b).unwrap();
    assert!(!s.graph_ref().contains(b), "gone to its owner at once");
    let _ = s.update();
    let tail = frames(&mut s, SPAN + 64);
    assert!(tail[0] > 1.9, "still heard while it fades: {}", tail[0]);
    assert!(tail[SPAN..].iter().all(|&x| x == 0.0));
    let _ = s.update();
    let _ = s.update();
    assert_eq!(s.graph_ref().node_count(), 1, "only a is left");
}
