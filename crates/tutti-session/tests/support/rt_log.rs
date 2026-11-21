//! A node that logs on the audio thread, and a `tracing` collector for the
//! `tutti::` events a session emits for it.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Io, Node, Prepare, RtLevel, RtValue, Shape, Status};
use tutti_units::Tail;

pub static SITE: &str = "non-finite sample";

/// A silent source that logs a warning at frame 7 of every block, three
/// times (one record, two repeats), and a debug line.
#[derive(Clone)]
pub struct Noisy;

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
pub struct Logged {
    pub level: Option<Level>,
    pub target: String,
    pub message: String,
    pub node: String,
    pub frame: u64,
    pub repeats: u64,
    pub fields: String,
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

#[allow(dead_code, reason = "a binary uses one of the two collectors")]
pub fn rt_events(f: impl FnOnce()) -> Vec<Logged> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Collect(seen.clone()));
    tracing::subscriber::with_default(subscriber, f);
    let all = std::mem::take(&mut *seen.lock().expect("collected"));
    all.into_iter()
        .filter(|e| e.target.starts_with("tutti::"))
        .collect()
}

/// As [`rt_events`], for work that logs on a thread `f` does not run on (a bounce renders as
/// a task of the session's executor): the collector is the process's default subscriber, so
/// one test of a process may call it, and a test's own filter picks its records out.
#[allow(dead_code, reason = "a binary uses one of the two collectors")]
pub fn rt_events_on_any_thread(f: impl FnOnce()) -> Vec<Logged> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Collect(seen.clone()));
    tracing::subscriber::set_global_default(subscriber)
        .expect("one test of the process installs the default subscriber");
    f();
    let all = std::mem::take(&mut *seen.lock().expect("collected"));
    all.into_iter()
        .filter(|e| e.target.starts_with("tutti::"))
        .collect()
}
