//! [`OfflineWait`] — what an offline fork's batcher waits on, and what it
//! latches when the wait fails.

use crate::ServerWatch;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tutti_plugin_types::{PluginFailure, RateRefusal, WaitSwitch};
use tutti_rt::Mutex;
use tutti_units::Samples;

/// An offline fork's wait for the server: the per-block budget, and the
/// latches the batcher sets when the wait fails (a missed budget, a plugin that
/// refused its rate). A dead server is the fork's [`ServerWatch`], the one latch
/// every signal of the death sets.
///
/// Whether the pipeline waits at all is the node's: the wait is armed through
/// [`switch`](Self::switch) by the node's `prepare` alone, for a graph prepared
/// offline. The plugin's own render mode is separate and never arms it.
///
/// The batcher records the failure here and whoever probes the fork's health
/// reads it back ([`gave_up`](Self::gave_up), [`death`](Self::death),
/// [`refusal`](Self::refusal)): a render that ends before the next process
/// poll drops the bridge with the executor, so the latch is what outlives it.
pub struct OfflineWait {
    /// Whether the fork's server is alive. Holds no process: a probe the editor
    /// keeps does not keep the fork alive after its node is gone.
    server: Arc<ServerWatch>,
    /// Latched when a block first misses `budget`.
    gave_up: AtomicBool,
    /// The plugin's standing rate refusal, offered by the bridge thread
    /// (`offer_refusal`) before it raises its flag; filled, never emptied.
    /// Written and freed only off the audio thread.
    refusal: Mutex<Option<RateRefusal>>,
    /// Latched by the audio thread when the fork rendered silence because of a
    /// refusal: a scalar store, no lock, no clone. Once set, the slot holds a
    /// refusal, since it was filled before the bridge's flag rose.
    refused: AtomicBool,
    /// The latency the fork's graph was compiled against, recorded once the
    /// plugin has settled (`usize::MAX`: not planned yet).
    planned: AtomicUsize,
    /// Whether the pipeline waits for each chunk: written only by the node's
    /// `prepare` from the graph's render mode ([`switch`](Self::switch)), so a
    /// fork prepared for realtime never blocks the audio thread.
    armed: WaitSwitch,
    budget: Duration,
}

impl std::fmt::Debug for OfflineWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineWait")
            .field("budget", &self.budget)
            .field("gave_up", &self.gave_up)
            .finish_non_exhaustive()
    }
}

impl OfflineWait {
    /// A wait of `budget` per chunk on the server `server` watches, with
    /// nothing latched.
    pub fn new(budget: Duration, server: Arc<ServerWatch>) -> Self {
        Self {
            server,
            gave_up: AtomicBool::new(false),
            refusal: Mutex::new(None),
            refused: AtomicBool::new(false),
            planned: AtomicUsize::new(usize::MAX),
            armed: WaitSwitch::new(),
            budget,
        }
    }

    /// Whether the pipeline waits for each chunk, up to
    /// [`budget`](Self::budget). Not until the node's `prepare` in an offline
    /// graph arms it. The plugin's own render mode has no say.
    pub fn armed(&self) -> bool {
        self.armed.armed()
    }

    /// The switch the node's `prepare` drives: the only source of the wait. A
    /// clone is the same switch.
    #[must_use]
    pub fn switch(&self) -> WaitSwitch {
        self.armed.clone()
    }

    /// Record the whole latency (the plugin's own and the pipeline's chunk)
    /// the fork's graph is compiled against: what the node declares right
    /// after this `prepare`, once the plugin has settled.
    pub fn plan(&self, latency: Samples) {
        self.planned.store(latency.get(), Ordering::Release);
    }

    /// The latency recorded by [`plan`](Self::plan), or `None` before one.
    pub fn planned(&self) -> Option<Samples> {
        match self.planned.load(Ordering::Acquire) {
            usize::MAX => None,
            planned => Some(Samples::new(planned)),
        }
    }

    /// The per-block budget (`Timeouts::call`).
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// How long the bridge thread holds a block for its reply while this wait
    /// is in force: the budget while armed, until a block has missed it or the
    /// server has died, then `None` (the live bound: nothing is awaited past a
    /// few block periods, so a hung server costs one budget, not one per
    /// chunk).
    pub(crate) fn reply_wait(&self) -> Option<Duration> {
        (self.armed() && !self.stopped_waiting()).then_some(self.budget)
    }

    /// Whether the wait should not wait at all any more: a block already
    /// missed its budget, or the server is dead.
    pub(crate) fn stopped_waiting(&self) -> bool {
        self.gave_up() || self.server.is_dead()
    }

    /// Whether a block missed its budget.
    pub fn gave_up(&self) -> bool {
        self.gave_up.load(Ordering::Acquire)
    }

    /// Latch a missed budget.
    pub(crate) fn give_up(&self) {
        self.gave_up.store(true, Ordering::Release);
    }

    /// The server's death, if the watch has one: the first signal's cause.
    pub fn death(&self) -> Option<PluginFailure> {
        self.server.cause()
    }

    /// The rate refusal the fork rendered silence for, if any: the latest
    /// one offered, once the audio thread has latched.
    pub fn refusal(&self) -> Option<RateRefusal> {
        if !self.refused.load(Ordering::Acquire) {
            return None;
        }
        self.refusal.lock().clone()
    }

    /// The bridge thread's record of a plugin refusal, called before the
    /// bridge's flag is raised, so the audio thread never sees the flag
    /// without a reason. The slot is only ever filled, never emptied: a
    /// clearing is not offered, and whether a refusal is reported is the
    /// latch's. A refusal offered again replaces the held one (both are real
    /// refusals of this fork), also after the latch; the replaced value drops
    /// here, on the bridge thread.
    pub(crate) fn offer_refusal(&self, refusal: RateRefusal) {
        let old = self.refusal.lock().replace(refusal);
        drop(old);
    }

    /// Latch that the fork renders silence for a rate refusal, for the rest of
    /// the render. Audio-thread safe: one atomic store.
    pub(crate) fn latch_refusal(&self) {
        self.refused.store(true, Ordering::Release);
    }

    /// Whether the server is gone, by any signal the watch has had: its
    /// stream ending, its process ending, or the host ending it.
    pub fn server_died(&self) -> bool {
        self.server.is_dead()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait() -> OfflineWait {
        OfflineWait::new(Duration::from_secs(7), Arc::new(ServerWatch::new()))
    }

    /// An armed wait, as an offline graph's prepare leaves it.
    fn armed() -> OfflineWait {
        let wait = wait();
        wait.switch().arm(true);
        wait
    }

    /// The bridge holds a block for the budget while the wait is armed, and
    /// holds none while it is not.
    #[test]
    fn a_wait_in_force_holds_the_bridge_for_the_budget() {
        assert_eq!(armed().reply_wait(), Some(Duration::from_secs(7)));
        assert_eq!(wait().reply_wait(), None, "not armed: the live bound");
    }

    /// One missed budget returns the bridge to the live bound for good, so a
    /// reactivation that reads the latch stays off the budget too.
    #[test]
    fn a_missed_budget_returns_the_bridge_to_the_live_bound() {
        let wait = armed();
        wait.give_up();
        assert_eq!(wait.reply_wait(), None);
    }

    /// **A refusal that was offered and then latched is still
    /// reported**: the slot is never emptied, so a latch that lands between the
    /// bridge's clearing and its flag still has its reason. One never latched
    /// reports nothing.
    #[test]
    fn a_refusal_cleared_before_the_latch_is_still_reported_once_latched() {
        use tutti_plugin_types::RateRefusalCause;
        let wait = wait();
        let refusal = RateRefusal::new(
            tutti_units::SampleRate::SR_96K,
            RateRefusalCause::Unconfigured,
        );
        wait.offer_refusal(refusal.clone());
        assert_eq!(wait.refusal(), None, "not latched, not reported");
        wait.latch_refusal();
        assert_eq!(wait.refusal(), Some(refusal));
    }
}
