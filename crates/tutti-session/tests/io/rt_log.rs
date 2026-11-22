//! A node's `Io::log` on the audio thread reaches the session's `tracing`
//! subscriber as an event on `tutti::graph`, with the node and the frame the
//! executor filled in, on the session's next update.

use tracing::Level;
use tutti_buffers::ChannelLayout;
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_graph::ForkByClone;
use tutti_session::{MainOutput, OutputPort, Session};
use tutti_units::{SampleRate, Samples};

#[path = "../support/rt_log.rs"]
mod support;
use support::{Noisy, SITE, rt_events};

#[test]
fn a_node_that_logs_on_the_audio_thread_emits_a_tracing_event_on_the_next_update() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(
            OutputSpec::new(
                SampleRate::of(48_000.0),
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device");
    let noisy = session.graph().add(ForkByClone(Noisy));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(noisy, 0)])
        .unwrap()
        .unfaded();

    let events = rt_events(|| {
        let _ = session.update();
        // The first update stored the subscriber's filter: the block after it
        // logs.
        stream
            .render_block(Samples::new(64))
            .expect("the stream is open");
        let _ = session.update();
    });

    let warn: Vec<_> = events
        .iter()
        .filter(|e| e.level == Some(Level::WARN))
        .collect();
    assert_eq!(warn.len(), 1, "{events:?}");
    let event = warn[0];
    assert_eq!(event.target, "tutti::graph");
    assert_eq!(event.message, SITE);
    assert_eq!((event.frame, event.repeats), (7, 2));
    assert_eq!(event.fields, "channel=1");
    assert_eq!(
        event.node,
        format!(
            "{}#{}",
            std::any::type_name::<Noisy>(),
            tutti_graph::NodeKey::from(noisy).0
        ),
        "the node is named by its kind and id"
    );
    assert!(
        events.iter().any(|e| e.level == Some(Level::DEBUG)),
        "a debug record is emitted when the subscriber takes debug: {events:?}"
    );
}

#[test]
fn nothing_is_logged_when_no_subscriber_takes_the_target() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(
            OutputSpec::new(
                SampleRate::of(48_000.0),
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device");
    let noisy = session.graph().add(ForkByClone(Noisy));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(noisy, 0)])
        .unwrap()
        .unfaded();
    let _ = session.update();
    stream
        .render_block(Samples::new(64))
        .expect("the stream is open");
    let events = rt_events(|| {
        let _ = session.update();
    });
    assert!(events.is_empty(), "{events:?}");
}
