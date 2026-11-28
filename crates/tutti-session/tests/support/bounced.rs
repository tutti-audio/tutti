//! Starts a bounce, awaits its render and applies it: the one way a test reads a result.

#![allow(dead_code, reason = "each area binary uses the helpers it needs")]

use tutti_dsp::FftFloat;
use tutti_fixtures::block_on;
use tutti_graph::NodeKey;
use tutti_session::{AppKey, Bounce, BounceError, Bounced, Clock, Destination, Session};

/// `bounce` rendered on the session's executor and applied to `session`.
pub(crate) fn bounced<K: AppKey, C: Clock, M: Destination<C::Float>>(
    session: &mut Session<K, C>,
    bounce: Bounce<NodeKey, M, C::Float>,
) -> Result<Bounced<M, C::Float>, BounceError>
where
    C::Float: FftFloat,
{
    let rendered = block_on(session.bounce(bounce)?);
    session.apply(rendered)
}

/// A bounce to memory of the mix's first `frames` frames at the session's rate, on a
/// frozen clock, keeping the graph's own latency and no tail: the graph as it
/// stands when the session starts it, forked and rendered from a reset.
pub(crate) fn first_frames(frames: u64) -> Bounce<NodeKey, tutti_session::ToMemory> {
    Bounce::new()
        .main_output(tutti_session::ToMemory::new())
        .range(tutti_session::Range::Frames(
            tutti_units::Frame::ZERO,
            tutti_units::Frame::new(frames),
        ))
        .clock(tutti_session::ExportClock::frozen())
        .latency(tutti_session::BounceLatency::Keep)
        .tail(tutti_session::BounceTail::None)
}
