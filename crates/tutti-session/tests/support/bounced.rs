//! Starts a bounce, awaits its render and applies it: the one way a test reads a result.
//!
//! A macro, because the trait that lets a function start any destination's bounce is not
//! nameable outside the crate: a generic helper could not bound on it.

/// `bounce` rendered on the session's executor and applied to `session`: a
/// `Result<Bounced<D, F>, BounceError>`.
macro_rules! bounced {
    ($session:expr, $bounce:expr $(,)?) => {{
        let session = $session;
        match session.bounce($bounce) {
            Ok(job) => {
                let rendered = tutti_fixtures::block_on(job);
                session.apply(rendered)
            }
            Err(e) => Err(e),
        }
    }};
}

pub(crate) use bounced;
