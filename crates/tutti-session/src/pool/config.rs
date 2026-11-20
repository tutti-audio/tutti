//! How the pool runs its loads.

use std::num::NonZeroUsize;

/// How the session runs the loads the pool starts
/// ([`SessionBuilder::pool_config`](crate::SessionBuilder::pool_config)).
///
/// Start from [`new`](Self::new), the default, and change what you need:
/// more knobs may come, so the struct is built through its methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolConfig {
    /// How many worker stages run at once. The rest wait, in declaration
    /// order, and start as running ones land. Stages on the main thread or the
    /// session's own thread are not counted: they run in
    /// [`update`](crate::Session::update). Default 4.
    pub(super) parallel_loads: NonZeroUsize,
    /// How many of the newest snapshot holds keep their entries live: a count
    /// of holds, not of distinct entries, so several holds on one entry count
    /// several. A released entry kept only by older holds is parked: a kind
    /// with something live behind it releases it
    /// ([`PoolEntry::retain`](crate::PoolEntry::retain)), and recalling it loads
    /// again. A node's hold never parks. Default 8.
    pub(super) live_holds: usize,
}

impl PoolConfig {
    /// The defaults: four worker stages at once, eight live holds.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            parallel_loads: NonZeroUsize::MIN.saturating_add(3),
            live_holds: 8,
        }
    }

    /// At most `loads` worker stages run at once.
    #[must_use]
    pub const fn parallel_loads(mut self, loads: NonZeroUsize) -> Self {
        self.parallel_loads = loads;
        self
    }

    /// The newest `holds` snapshot holds keep their entries live.
    #[must_use]
    pub const fn live_holds(mut self, holds: usize) -> Self {
        self.live_holds = holds;
        self
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self::new()
    }
}
