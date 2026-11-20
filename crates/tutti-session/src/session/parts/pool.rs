//! [`PoolPart`]: what the app declared loaded, and the source of the ids of the resources
//! the session holds.

use crate::peer::ResourceIds;
use crate::{AppKey, Clock, Pool, PoolConfig};

/// The pool's entries, and the source of the ids that name the session's resources.
#[derive(Debug)]
pub(crate) struct PoolPart<K: AppKey, C: Clock> {
    /// What the app declared loaded ([`Session::pool`](crate::Session::pool)).
    pub(crate) entries: Pool<K, C>,
    /// The source of the ids of this session's resources.
    #[cfg_attr(
        not(any(test, feature = "plugin", feature = "midi")),
        expect(
            dead_code,
            reason = "the ids of plugins and MIDI ports (features `plugin`, `midi`)"
        )
    )]
    pub(crate) resource_ids: ResourceIds,
}

impl<K: AppKey, C: Clock> PoolPart<K, C> {
    pub(crate) fn new(config: PoolConfig) -> Self {
        Self {
            entries: Pool::new(config),
            resource_ids: ResourceIds::new(),
        }
    }

    /// The same part on the clock `N`; the runs waiting for an entry have
    /// been run or refused by the caller.
    #[cfg(feature = "device")]
    pub(crate) fn rebind<N: Clock<Float = C::Float>>(self) -> PoolPart<K, N> {
        PoolPart {
            entries: self.entries.rebind(),
            resource_ids: self.resource_ids,
        }
    }
}
