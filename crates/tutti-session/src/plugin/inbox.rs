//! The plugin inbox: where the loops tutti runs for a hosted plugin (a crash
//! recovery's reload, a parameter mirror's read, a state snapshot) report to the
//! session.
//!
//! Each loop is a blocking round trip on the session's executor
//! ([`PluginPart::spawn`](crate::session::parts::PluginPart::spawn)). Its task holds a
//! [`Post`], a `Sender` clone, and no executor: it sends one [`Arrival`] when the round
//! trip returns, and `Phase::Plugins` drains the [`Inbox`] before it decides what is due,
//! so what a task sent lands in the update that finds it. Every arrival carries the token
//! the node's record issued with the ask; a landing whose token is not the record's any
//! more (the plugin was crossfaded or removed since) lands nowhere.
//!
//! A task whose session is gone sends into a closed channel and ends silently. A reload
//! always ends in an arrival: [`ReloadPost`] sends [`Arrival::ReloadEnded`] when its task
//! is dropped unrun or panics.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use tutti_graph::NodeKey;
use tutti_platform::Instant;
use tutti_plugin_types::StateError;

use super::{ParamValues, PluginLoaded};
use crate::RecoveryFailure;
use crate::jobs::panic_message;

/// What a loop's task ended with.
pub(crate) enum Arrival {
    /// A reload ended with its plugin.
    Reload {
        node: NodeKey,
        token: Arc<()>,
        snapshot_age: Option<Duration>,
        loaded: Box<PluginLoaded>,
    },
    /// A reload ended without one: its task was dropped or panicked.
    ReloadEnded {
        node: NodeKey,
        token: Arc<()>,
        cause: RecoveryFailure,
    },
    /// A parameter read ended with its values.
    Params {
        node: NodeKey,
        token: Arc<()>,
        values: ParamValues,
    },
    /// A state snapshot ended with its state.
    Snapshot {
        node: NodeKey,
        token: Arc<()>,
        asked: Instant,
        state: Result<Vec<u8>, StateError>,
    },
}

/// The session's end of the plugin loops: the receiver `Phase::Plugins` drains.
pub(crate) struct Inbox {
    pub(super) rx: Receiver<Arrival>,
    pub(super) tx: Sender<Arrival>,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox").finish_non_exhaustive()
    }
}

impl Inbox {
    pub(crate) fn new() -> Self {
        let (tx, rx) = channel();
        Self { rx, tx }
    }

    /// The sending end a task holds.
    pub(crate) fn post(&self) -> Post {
        Post(self.tx.clone())
    }

    /// Everything that arrived, in arrival order.
    pub(crate) fn drain(&self) -> Vec<Arrival> {
        self.rx.try_iter().collect()
    }
}

/// A task's way back to the session: sends never block, and once the session is gone
/// they drop what they carry.
#[derive(Clone)]
pub(crate) struct Post(Sender<Arrival>);

impl Post {
    /// A parameter read's end.
    pub(crate) fn params(&self, node: NodeKey, token: Arc<()>, values: ParamValues) {
        let _ = self.0.send(Arrival::Params {
            node,
            token,
            values,
        });
    }

    /// A state snapshot's end.
    pub(crate) fn snapshot(
        &self,
        node: NodeKey,
        token: Arc<()>,
        asked: Instant,
        state: Result<Vec<u8>, StateError>,
    ) {
        let _ = self.0.send(Arrival::Snapshot {
            node,
            token,
            asked,
            state,
        });
    }

    /// A reload's way back: sends its end exactly once.
    pub(crate) fn reload(
        &self,
        node: NodeKey,
        token: Arc<()>,
        snapshot_age: Option<Duration>,
    ) -> ReloadPost {
        ReloadPost {
            tx: self.0.clone(),
            node,
            token,
            snapshot_age,
            sent: false,
        }
    }
}

/// A reload task's one end: [`run`](Self::run) sends the load's result, or the panic that
/// ended it, and a task dropped before it ran sends [`RecoveryFailure::ReloadDropped`].
#[must_use = "a reload that is never run ends as dropped"]
pub(crate) struct ReloadPost {
    tx: Sender<Arrival>,
    node: NodeKey,
    token: Arc<()>,
    snapshot_age: Option<Duration>,
    sent: bool,
}

impl ReloadPost {
    /// Runs `load` and sends what it returned, or [`RecoveryFailure::ReloadPanicked`]
    /// when it panicked.
    pub(crate) fn run(mut self, load: impl FnOnce() -> PluginLoaded) {
        let arrival = match catch_unwind(AssertUnwindSafe(load)) {
            Ok(loaded) => Arrival::Reload {
                node: self.node,
                token: Arc::clone(&self.token),
                snapshot_age: self.snapshot_age,
                loaded: Box::new(loaded),
            },
            Err(payload) => self.ended(RecoveryFailure::ReloadPanicked {
                message: panic_message(payload.as_ref()),
            }),
        };
        self.sent = true;
        let _ = self.tx.send(arrival);
    }

    fn ended(&self, cause: RecoveryFailure) -> Arrival {
        Arrival::ReloadEnded {
            node: self.node,
            token: Arc::clone(&self.token),
            cause,
        }
    }
}

impl Drop for ReloadPost {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send(self.ended(RecoveryFailure::ReloadDropped));
        }
    }
}

/// Runs a loop's round trip, warning when it panics: the token it held drops with it,
/// which frees the plugin for its next ask.
pub(crate) fn contained<R>(what: &str, node: NodeKey, work: impl FnOnce() -> R) -> Option<R> {
    match catch_unwind(AssertUnwindSafe(work)) {
        Ok(done) => Some(done),
        Err(payload) => {
            tracing::warn!(
                "plugin {node:?}: its {what} panicked: {}",
                panic_message(payload.as_ref())
            );
            None
        }
    }
}
