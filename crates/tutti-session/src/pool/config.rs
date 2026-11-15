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
    pub parallel_loads: NonZeroUsize,
}

impl PoolConfig {
    /// The defaults: four worker stages at once.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            parallel_loads: NonZeroUsize::MIN.saturating_add(3),
        }
    }

    /// At most `loads` worker stages run at once.
    #[must_use]
    pub const fn with_parallel_loads(mut self, loads: NonZeroUsize) -> Self {
        self.parallel_loads = loads;
        self
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self::new()
    }
}
