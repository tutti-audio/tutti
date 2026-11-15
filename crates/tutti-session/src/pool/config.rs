//! How the pool runs its loads.

use std::num::NonZeroUsize;

/// How the session runs the loads the pool starts
/// ([`SessionBuilder::pool_config`](crate::SessionBuilder::pool_config)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    /// How many worker stages run at once. The rest wait, in declaration
    /// order, and start as running ones land. Stages on the main thread or the
    /// session's own thread are not counted: they run in
    /// [`update`](crate::Session::update). Default 4.
    pub parallel_loads: NonZeroUsize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            parallel_loads: NonZeroUsize::new(4).expect("four is not zero"),
        }
    }
}
