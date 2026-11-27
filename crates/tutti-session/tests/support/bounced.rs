//! Starts a bounce, awaits its render and applies it: the one way a test reads a result.

use tutti_dsp::FftFloat;
use tutti_fixtures::block_on;
use tutti_graph::NodeKey;
use tutti_session::{AppKey, Bounce, BounceError, Bounced, Clock, Destination, Session};

/// `bounce` rendered on the session's executor and applied to `session`.
pub(crate) fn bounced<K: AppKey, C: Clock, M: Destination>(
    session: &mut Session<K, C>,
    bounce: Bounce<NodeKey, M, C::Float>,
) -> Result<Bounced<M, C::Float>, BounceError>
where
    C::Float: FftFloat,
{
    let rendered = block_on(session.bounce(bounce)?);
    session.apply(rendered)
}
