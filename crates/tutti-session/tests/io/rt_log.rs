//! A node's `Io::log` on the audio thread reaches the session's `tracing`
//! subscriber as an event on `tutti::rt`, with the node and the frame the
//! executor filled in, on the session's next update.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_graph::{Cx, Io, Node, Prepare, RtLevel, RtValue, Shape, Status, Unforkable};
use tutti_session::{OutPort, Root, Session};
use tutti_types::{ChannelLayout, SampleRate, Tail};

static SITE: &str = "non-finite sample";

/// A silent source that logs a warning at frame 7 of every block, three
/// times (one record, two repeats), and a debug line.
struct Noisy;

impl Node for Noisy {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let at = io.offset(7).expect("inside the block");
        for _ in 0..3 {
            io.log(
                RtLevel::Warn,
                SITE,
                at,
                &[("channel", RtValue::from(1usize))],
            );
        }
        io.log(RtLevel::Debug, "detail", at, &[]);
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Logged {
    level: Option<Level>,
    target: String,
    message: String,
    node: String,
    frame: u64,
    repeats: u64,
    fields: String,
}

#[derive(Default)]
struct Read(Logged);

impl Visit for Read {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "frame" => self.0.frame = value,
            "repeats" => self.0.repeats = value,
            _ => {}
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        match field.name() {
            "message" => self.0.message = text,
            "node" => self.0.node = text,
            "fields" => self.0.fields = text,
            _ => {}
        }
    }
}

struct Collect(Arc<Mutex<Vec<Logged>>>);

impl<S: Subscriber> Layer<S> for Collect {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut read = Read::default();
        event.record(&mut read);
        let mut logged = read.0;
        logged.level = Some(*event.metadata().level());
        logged.target = event.metadata().target().to_owned();
        self.0.lock().expect("collected").push(logged);
    }
}

fn rt_events(f: impl FnOnce()) -> Vec<Logged> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Collect(seen.clone()));
    tracing::subscriber::with_default(subscriber, f);
    let all = std::mem::take(&mut *seen.lock().expect("collected"));
    all.into_iter()
        .filter(|e| e.target.starts_with("tutti::"))
        .collect()
}

#[test]
fn a_node_that_logs_on_the_audio_thread_emits_a_tracing_event_on_the_next_update() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(Default::default())
        .build_device_on(
            OutputSpec::new(
                SampleRate::of(48_000.0),
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device");
    let (noisy, _) = session.graph().add(Unforkable(Noisy));
    session
        .graph()
        .set_sources_hard(Root::output(0), [OutPort::new(noisy, 0)])
        .unwrap();

    let events = rt_events(|| {
        let _ = session.update();
        // The first update stored the subscriber's filter: the block after it
        // logs.
        stream.render_block(64).expect("the stream is open");
        let _ = session.update();
    });

    let warn: Vec<_> = events
        .iter()
        .filter(|e| e.level == Some(Level::WARN))
        .collect();
    assert_eq!(warn.len(), 1, "{events:?}");
    let event = warn[0];
    assert_eq!(event.target, "tutti::rt");
    assert_eq!(event.message, SITE);
    assert_eq!((event.frame, event.repeats), (7, 2));
    assert_eq!(event.fields, "channel=1");
    assert!(
        event.node.contains('#'),
        "the node is named by its kind and id: {}",
        event.node
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
        .device(Default::default())
        .build_device_on(
            OutputSpec::new(
                SampleRate::of(48_000.0),
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device");
    let (noisy, _) = session.graph().add(Unforkable(Noisy));
    session
        .graph()
        .set_sources_hard(Root::output(0), [OutPort::new(noisy, 0)])
        .unwrap();
    let _ = session.update();
    stream.render_block(64).expect("the stream is open");
    let events = rt_events(|| {
        let _ = session.update();
    });
    assert!(events.is_empty(), "{events:?}");
}
