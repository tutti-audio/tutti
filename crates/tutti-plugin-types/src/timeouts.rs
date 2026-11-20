//! [`Timeouts`]: every deadline of a served plugin's host side, in one record.
//!
//! The bridge thread waits on the server for each of these, and the host
//! threads wait on the bridge for the answer, so the two halves must agree on
//! one set of figures. A host wait is always its bridge deadline plus a margin
//! ([`param_wait`](Timeouts::param_wait), [`editor_wait`](Timeouts::editor_wait),
//! [`settle_wait`](Timeouts::settle_wait), [`state_wait`](Timeouts::state_wait)),
//! never the bridge's own figure: a host that gave up at the deadline would give
//! up as the answer arrived.

use std::time::Duration;

use thiserror::Error;

/// How long each wait of the plugin host runs before it gives up.
///
/// [`Default`] is what a host runs with unless it chooses otherwise; each field
/// is the deadline of one kind of wait and none is a total (a state transfer's
/// is the gap between chunks).
///
/// Non-exhaustive, so a deadline can be added: build one from the default and
/// set the fields to change (`let mut t = Timeouts::default(); t.call = ..;`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Timeouts {
    /// A reply during the launch handshake, and one chunk of an offline fork's
    /// render. The generic wait on the server: 5 s.
    pub call: Duration,
    /// The server opening the plugin (its library, instance and initialisation)
    /// after a `LoadPlugin`, which a sampler or a large synth spends seconds on:
    /// 60 s.
    pub load: Duration,
    /// How long a server that was told to shut down is given to end by itself
    /// (the plugin's own teardown runs in it) before it is killed: 2 s.
    pub shutdown: Duration,
    /// The server's socket coming up after the process spawned. A slow machine
    /// under load is the failure this guards; a server that died is reported
    /// at once whatever this is: 5 s.
    pub connect: Duration,
    /// The server restoring a staged state once it said it began. One opaque
    /// plugin call with no progress inside it to measure, so one flat and
    /// generous window: 60 s.
    pub state_restore: Duration,
    /// How long the server has to save or restore a state, from the request,
    /// before the request is declared stalled: 10 s.
    pub state: Duration,
    /// The server's answer to a query (a parameter, a preset, the parameter
    /// list): 5 s.
    pub param: Duration,
    /// The server opening or resizing its editor, which a large plugin builds
    /// in the call: 10 s.
    pub editor: Duration,
    /// A plugin re-activating at a new sample rate, which rebuilds its
    /// processing: 10 s. Longer than [`param`](Self::param).
    pub rate: Duration,
    /// Finishing a reply that has already begun to arrive when its request's
    /// own budget runs out: 500 ms.
    pub drain: Duration,
    /// What a host adds to a bridge deadline before it gives up on the answer:
    /// the bridge answers at its deadline, so the host must outlast it. 1 s.
    pub reply_margin: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            call: Duration::from_secs(5),
            load: Duration::from_secs(60),
            shutdown: Duration::from_secs(2),
            connect: Duration::from_secs(5),
            state_restore: Duration::from_secs(60),
            state: Duration::from_secs(10),
            param: Duration::from_secs(5),
            editor: Duration::from_secs(10),
            rate: Duration::from_secs(10),
            drain: Duration::from_millis(500),
            reply_margin: Duration::from_secs(1),
        }
    }
}

/// A [`Timeouts`] that cannot work.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("the timeout `{field}` is zero: a wait of no time can never see an answer")]
pub struct ZeroTimeout {
    /// The field that is zero.
    pub field: &'static str,
}

impl Timeouts {
    /// The host's wait for a query's answer: the bridge's [`param`](Self::param)
    /// deadline and the margin for the answer to arrive.
    #[must_use]
    pub fn param_wait(&self) -> Duration {
        self.param.saturating_add(self.reply_margin)
    }

    /// The host's wait for the editor to open or resize. The bridge answers
    /// with its own refusal at [`editor`](Self::editor), so this outlasts it by
    /// a query's whole deadline, which also covers a refusal queued behind
    /// another command.
    #[must_use]
    pub fn editor_wait(&self) -> Duration {
        self.editor.saturating_add(self.param)
    }

    /// The host's wait for the first query of a settle, which queues behind an
    /// outstanding rate change: the rate's deadline, then a query's wait.
    #[must_use]
    pub fn settle_wait(&self) -> Duration {
        self.rate.saturating_add(self.param_wait())
    }

    /// The most a state request may be kept waiting by the server's signs of
    /// life: ten windows. Without them the request is due at
    /// [`state`](Self::state).
    #[must_use]
    pub fn state_ceiling(&self) -> Duration {
        self.state.saturating_mul(10)
    }

    /// How long a caller waits for a state request's answer: the bridge's
    /// [`state_ceiling`](Self::state_ceiling) and the margin, so the bridge's own
    /// stall report arrives first.
    #[must_use]
    pub fn state_wait(&self) -> Duration {
        self.state_ceiling().saturating_add(self.reply_margin)
    }

    /// Whether every deadline can be waited on.
    ///
    /// # Errors
    ///
    /// A field is zero.
    pub fn validate(&self) -> Result<(), ZeroTimeout> {
        let fields = [
            ("call", self.call),
            ("load", self.load),
            ("shutdown", self.shutdown),
            ("connect", self.connect),
            ("state_restore", self.state_restore),
            ("state", self.state),
            ("param", self.param),
            ("editor", self.editor),
            ("rate", self.rate),
            ("drain", self.drain),
            ("reply_margin", self.reply_margin),
        ];
        match fields.into_iter().find(|(_, d)| d.is_zero()) {
            Some((field, _)) => Err(ZeroTimeout { field }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every host wait outlasts the bridge deadline it waits on**, for the
    /// defaults and for a shortened set: the ordering is by construction of
    /// the wait methods, so no choice of valid figures reverses it.
    #[test]
    fn a_host_wait_always_outlasts_its_bridge_deadline() {
        let short = Timeouts {
            call: Duration::from_millis(1),
            load: Duration::from_millis(1),
            shutdown: Duration::from_millis(1),
            connect: Duration::from_millis(1),
            state_restore: Duration::from_millis(1),
            state: Duration::from_millis(20),
            param: Duration::from_millis(30),
            editor: Duration::from_millis(40),
            rate: Duration::from_millis(50),
            drain: Duration::from_millis(1),
            reply_margin: Duration::from_millis(1),
        };
        for t in [Timeouts::default(), short] {
            assert_eq!(t.validate(), Ok(()));
            assert!(t.param_wait() > t.param, "a query's wait");
            assert!(t.editor_wait() > t.editor, "the editor's wait");
            assert!(
                t.settle_wait() > t.rate,
                "the settle waits out a rate change"
            );
            assert!(t.settle_wait() > t.rate + t.param, "and a query after it");
            assert!(t.state_wait() > t.state, "a stalled transfer");
        }
    }

    /// A zero field is refused by name, and the margin that makes the order
    /// strict is one of them.
    #[test]
    fn a_zero_deadline_is_refused_by_name() {
        for field in ["call", "param", "reply_margin"] {
            let mut t = Timeouts::default();
            match field {
                "call" => t.call = Duration::ZERO,
                "param" => t.param = Duration::ZERO,
                _ => t.reply_margin = Duration::ZERO,
            }
            assert_eq!(t.validate(), Err(ZeroTimeout { field }));
        }
    }

    /// The figures a host has always run with.
    #[test]
    fn the_defaults_are_the_figures_the_bridge_shipped_with() {
        let t = Timeouts::default();
        assert_eq!(t.call, Duration::from_secs(5));
        assert!(t.load > t.call, "an open outlasts a generic call");
        assert_eq!(t.state_restore, Duration::from_secs(60));
        assert_eq!(t.editor_wait(), Duration::from_secs(15));
        assert_eq!(t.settle_wait(), Duration::from_secs(16));
    }
}
