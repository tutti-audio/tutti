//! The audio thread's log, emitted as `tracing` events.
//!
//! Nodes log through `Io::log`; the engine's [`RtLogReaders`] holds what they
//! logged. On the session's reclaim sweep this module drains it and emits one
//! event per record on target `tutti::rt` (a node of tutti's) or
//! `tutti::plugin` (a hosted plugin's line), at the record's level, with the
//! fields `node` (the node's kind and id), `frame`, `repeats` and `fields`
//! (the record's own named values), and the call site's text or the plugin's
//! line as the message. Each drain first stores into the executor's gate the
//! most verbose level the subscriber takes per target, so a record nobody
//! would emit is never written.
//!
//! A rise in the records lost (a full ring, or more than four in one node
//! call) is one `warn!` naming the count.

use std::fmt::Write as _;

use tracing::Level;
use tracing::level_filters::LevelFilter;
use tutti_graph::{LogFilter, NodeKey, RtLevel, RtLogReaders, RtRecord, RtTarget, RtValue};

/// The most verbose level a `tracing` subscriber would emit on `$target`.
macro_rules! most_verbose {
    ($target:literal) => {
        if tracing::enabled!(target: $target, Level::TRACE) {
            Some(RtLevel::Trace)
        } else if tracing::enabled!(target: $target, Level::DEBUG) {
            Some(RtLevel::Debug)
        } else if tracing::enabled!(target: $target, Level::INFO) {
            Some(RtLevel::Info)
        } else if tracing::enabled!(target: $target, Level::WARN) {
            Some(RtLevel::Warn)
        } else if tracing::enabled!(target: $target, Level::ERROR) {
            Some(RtLevel::Error)
        } else {
            None
        }
    };
}

/// What the current subscriber takes, per target.
fn current_filter() -> LogFilter {
    if LevelFilter::current() == LevelFilter::OFF {
        return LogFilter::OFF;
    }
    LogFilter::new(most_verbose!("tutti::rt"), most_verbose!("tutti::plugin"))
}

/// One event on `$target` at the record's level: the call sites tracing's
/// callsite cache needs, one per target and level.
macro_rules! emit_event {
    ($target:literal, $record:expr, $node:expr, $fields:expr, $message:expr) => {{
        let (r, node, fields, message) = ($record, $node, $fields, $message);
        let frame = r.frame().get();
        let repeats = r.repeats();
        match r.level() {
            RtLevel::Error => {
                tracing::event!(target: $target, Level::ERROR, %node, frame, repeats, %fields, "{message}")
            }
            RtLevel::Warn => {
                tracing::event!(target: $target, Level::WARN, %node, frame, repeats, %fields, "{message}")
            }
            RtLevel::Info => {
                tracing::event!(target: $target, Level::INFO, %node, frame, repeats, %fields, "{message}")
            }
            RtLevel::Debug => {
                tracing::event!(target: $target, Level::DEBUG, %node, frame, repeats, %fields, "{message}")
            }
            RtLevel::Trace => {
                tracing::event!(target: $target, Level::TRACE, %node, frame, repeats, %fields, "{message}")
            }
        }
    }};
}

/// The record's values as `name=value` pairs.
fn render_fields(record: &RtRecord) -> String {
    let mut out = String::new();
    for (i, (name, value)) in record.fields().iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = match value {
            RtValue::I64(v) => write!(out, "{name}={v}"),
            RtValue::U64(v) => write!(out, "{name}={v}"),
            RtValue::F64(v) => write!(out, "{name}={v}"),
            RtValue::Bool(v) => write!(out, "{name}={v}"),
        };
    }
    out
}

/// Emits `record` as one event; `kind` is the session's name for its node.
fn emit(record: &RtRecord, kind: Option<&str>) {
    let node = match kind {
        Some(kind) => format!("{kind}#{}", record.source().0),
        None => format!("#{}", record.source().0),
    };
    let fields = render_fields(record);
    let message = record.text().unwrap_or(record.message());
    match record.target() {
        RtTarget::Rt => emit_event!("tutti::rt", record, node, fields, message),
        RtTarget::Plugin => emit_event!("tutti::plugin", record, node, fields, message),
    }
}

/// Drains `readers` into `tracing`: the subscriber's filter stored first,
/// each record emitted, a warning if records were lost. `kind_of` names a
/// node by its kind, for a node the graph still has.
pub(crate) fn drain<'a>(readers: &RtLogReaders, kind_of: impl Fn(NodeKey) -> Option<&'a str>) {
    let lost = readers.drain(current_filter(), |record| {
        emit(record, kind_of(record.source()));
    });
    warn_lost(lost);
}

/// One warning naming `lost` records, if there were any.
fn warn_lost(lost: u64) {
    if lost > 0 {
        tracing::warn!(
            target: "tutti::rt",
            lost,
            "the audio thread's log lost records (a full ring, or more than four in one node call)"
        );
    }
}

/// An export's sink: the same events a live drain emits, for the nodes the
/// live graph named when the fork was gathered.
#[cfg(feature = "export")]
#[derive(Debug)]
pub(crate) struct Emit {
    kinds: std::collections::BTreeMap<NodeKey, String>,
}

#[cfg(feature = "export")]
impl Emit {
    /// The kinds of the nodes `live` holds.
    pub(crate) fn of(live: &tutti_graph::Editor) -> Self {
        let kinds = live
            .spec()
            .topology
            .nodes
            .iter()
            .map(|(&key, spec)| (key, spec.kind.clone()))
            .collect();
        Self { kinds }
    }
}

#[cfg(feature = "export")]
impl tutti_export::RtLogSink for Emit {
    fn filter(&self) -> LogFilter {
        current_filter()
    }

    fn record(&self, record: &RtRecord) {
        emit(record, self.kinds.get(&record.source()).map(String::as_str));
    }

    fn lost(&self, count: u64) {
        warn_lost(count);
    }
}
