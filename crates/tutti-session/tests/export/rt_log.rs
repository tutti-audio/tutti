//! A bounce's graph logs on its own audio thread, and the records reach the
//! session's `tracing` subscriber as they do for the live graph: drained
//! between blocks on the thread that renders.

use tracing::Level;
use tutti_graph::ForkByClone;
use tutti_session::{Bounce, BounceLatency, ExportClock, OutPort, Range, Root, Session};
use tutti_types::{ChannelLayout, SampleRate, Seconds};

#[path = "../support/rt_log.rs"]
mod support;
use support::{Noisy, SITE, rt_events};

#[test]
fn a_node_that_logs_in_a_bounce_emits_a_tracing_event_through_the_export_path() {
    let mut session = Session::builder()
        .inputs(ChannelLayout::EMPTY)
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session.graph().set_sample_rate(SampleRate::of(48_000.0));
    let (noisy, _) = session.graph().add(ForkByClone(Noisy));
    session
        .graph()
        .set_sources_hard(Root::output(0), [OutPort::new(noisy, 0)])
        .unwrap();
    let _ = session.update();

    let events = rt_events(|| {
        let bounce = Bounce::project()
            .master_buffers()
            .range(Range::Seconds(Seconds(0.0), Seconds(0.05)))
            .sample_rate(SampleRate::of(48_000.0))
            .channels(ChannelLayout::STEREO)
            .clock(ExportClock::frozen())
            .latency(BounceLatency::Keep)
            .tail(tutti_session::BounceTail::None);
        session.bounce(bounce).expect("the bounce renders");
    });

    let warn: Vec<_> = events
        .iter()
        .filter(|e| e.level == Some(Level::WARN) && e.message == SITE)
        .collect();
    assert!(
        warn.len() >= 2,
        "one record a block of a 2,400-frame render, in frame order: {events:?}"
    );
    assert!(warn.windows(2).all(|w| w[0].frame <= w[1].frame));
    assert_eq!(warn[0].target, "tutti::rt");
    assert_eq!((warn[0].frame, warn[0].repeats), (7, 2));
    assert!(warn[0].node.contains('#'), "{}", warn[0].node);
}
