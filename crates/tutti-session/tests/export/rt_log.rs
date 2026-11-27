//! A bounce's graph logs on its own audio thread, and the records reach the
//! session's `tracing` subscriber as they do for the live graph: drained
//! between blocks on the thread that renders (a task of the session's executor).

use crate::bounced::bounced;
use tracing::Level;
use tutti_buffers::ChannelLayout;
use tutti_graph::ForkByClone;
use tutti_session::ToMemory;
use tutti_session::{Bounce, BounceLatency, ExportClock, MainOutput, OutputPort, Range, Session};
use tutti_units::{SampleRate, Seconds};

#[path = "../support/rt_log.rs"]
mod support;
use support::{Noisy, SITE, rt_events_on_any_thread};

#[test]
fn a_node_that_logs_in_a_bounce_emits_a_tracing_event_through_the_export_path() {
    let mut session = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session
        .graph()
        .set_sample_rate(SampleRate::of(48_000.0))
        .unwrap();
    let noisy = session.graph().add(ForkByClone(Noisy));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(noisy, 0)])
        .unwrap()
        .unfaded();
    let _ = session.update();

    let events = rt_events_on_any_thread(|| {
        let bounce = Bounce::new()
            .main_output(
                ToMemory::new().channels(tutti_buffers::ChannelSet::default_for(
                    ChannelLayout::STEREO,
                )),
            )
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.05)))
            .sample_rate(SampleRate::of(48_000.0))
            .clock(ExportClock::frozen())
            .latency(BounceLatency::Keep)
            .tail(tutti_session::BounceTail::None);
        bounced(&mut session, bounce).expect("the bounce renders");
    });

    let warn: Vec<_> = events
        .iter()
        .filter(|e| e.level == Some(Level::WARN) && e.message == SITE)
        .collect();
    // A 2,400-frame render in blocks of the export's maximum: one record a
    // block at its frame 7, none lost, none twice, in frame order.
    let block = tutti_export::GRAPH_MAX_BLOCK.get() as u64;
    let frames: Vec<u64> = warn.iter().map(|e| e.frame).collect();
    let expected: Vec<u64> = (0..2_400)
        .step_by(block as usize)
        .map(|start| start + 7)
        .collect();
    assert_eq!(frames, expected, "{events:?}");
    let node = format!(
        "{}#{}",
        std::any::type_name::<Noisy>(),
        tutti_graph::NodeKey::from(noisy).0
    );
    for event in &warn {
        assert_eq!(event.target, "tutti::graph");
        assert_eq!(event.repeats, 2);
        assert_eq!(
            event.node, node,
            "the bounce names the node as the live graph does"
        );
    }
}
