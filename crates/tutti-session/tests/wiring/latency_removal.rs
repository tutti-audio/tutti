//! The published latency shrinks when the latency-bearing node leaves.
//!
//! The figure is what the session publishes after the compiler's solve: the total, and the
//! per-channel compensation a source outside the graph seeks by.
//!
//! Two paths reach the outputs: one through a lookahead limiter, one dry. The
//! dry channel pre-rolls to match, which is the whole figure. Removing the
//! limiter takes the channel it fed to silence, and with nothing
//! latency-bearing left the published latency is nothing at all.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_nodes::{LimiterControls, LimiterNode};
use tutti_session::{InPort, OutPort, Root, Session};
use tutti_units::{Db, SampleRate, Samples};

#[test]
fn the_published_latency_shrinks_when_the_latency_bearing_node_leaves() {
    let (mut s, _bg) = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let (dry, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    let (src, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    let (lim, _) = s.graph().add(LimiterNode::<f32>::mono(LimiterControls {
        threshold: Db::new(-1.0),
        ceiling: Db::new(-0.3),
        ..LimiterControls::DEFAULT
    }));
    s.graph()
        .set_sources_unfaded(InPort::new(lim, 0), [OutPort::new(src, 0)])
        .unwrap();
    s.graph()
        .set_sources_unfaded(Root::output(0), [OutPort::new(lim, 0)])
        .unwrap();
    s.graph()
        .set_sources_unfaded(Root::output(1), [OutPort::new(dry, 0)])
        .unwrap();
    let _ = s.update();

    let before = s.latency();
    assert!(
        before > Samples::new(0),
        "the limiter reports latency, so the graph has a figure to display"
    );
    assert_eq!(
        s.compensation().to_vec().as_slice(),
        [Samples::new(0), before],
        "the dry channel pre-rolls by the limiter's lookahead"
    );

    s.remove(lim).unwrap();
    // The limiter's output fades out over 256 frames before the node goes,
    // and the figure with it.
    for _ in 0..6 {
        for _ in 0..64 {
            s.render_frame(InterleavedMut::one_frame(&mut [0.0; 2]).unwrap());
        }
        let _ = s.update();
    }
    let _ = s.update();
    assert!(s.latency() < before, "the figure shrank");
    assert_eq!(
        s.latency(),
        Samples::new(0),
        "and with nothing latency-bearing left, to nothing at all"
    );
    assert_eq!(
        s.compensation().to_vec().as_slice(),
        [Samples::new(0), Samples::new(0)]
    );
}
