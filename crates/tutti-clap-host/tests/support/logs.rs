//! A `tracing` collector for the `tutti::plugin` events a host emits.

use std::fmt::Debug;
use std::sync::Arc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tutti_rt::Mutex;

/// One event: its level, target, message and the `plugin` and `severity` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub level: Level,
    pub target: String,
    pub message: String,
    pub plugin: Option<String>,
    pub severity: Option<i64>,
}

#[derive(Default)]
struct Fields(Logged);

impl Default for Logged {
    fn default() -> Self {
        Self {
            level: Level::TRACE,
            target: String::new(),
            message: String::new(),
            plugin: None,
            severity: None,
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "plugin" {
            self.0.plugin = Some(value.to_owned());
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "severity" {
            self.0.severity = Some(value);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.0.message = format!("{value:?}");
        }
    }
}

struct Collect(Arc<Mutex<Vec<Logged>>>);

impl<S: Subscriber> Layer<S> for Collect {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut logged = fields.0;
        logged.level = *event.metadata().level();
        logged.target = event.metadata().target().to_owned();
        self.0.lock().push(logged);
    }
}

/// Runs `f` on this thread and returns the events it emitted on target
/// `tutti::plugin`, in order.
pub fn plugin_events(f: impl FnOnce()) -> Vec<Logged> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Collect(seen.clone()));
    tracing::subscriber::with_default(subscriber, f);
    let all = std::mem::take(&mut *seen.lock());
    all.into_iter()
        .filter(|e| e.target == "tutti::plugin")
        .collect()
}
