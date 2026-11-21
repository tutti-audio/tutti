//! Whether tutti can run its background work: the one thread that drives the session's
//! executor, and the platforms that have none. The platform's difference is this one
//! module's.

pub(crate) use os::{Running, THREADS, start};

#[cfg_attr(not(target_arch = "wasm32"), path = "background/native.rs")]
#[cfg_attr(target_arch = "wasm32", path = "background/web.rs")]
mod os;

/// Fails with [`Error::NeedsThreads`](crate::Error::NeedsThreads) naming `work` where
/// this platform starts no thread, so a build that needs it never half-starts.
///
/// # Errors
///
/// `work`, on a target that starts no thread.
pub(crate) fn require(work: crate::BackgroundWork) -> crate::Result<()> {
    if THREADS {
        Ok(())
    } else {
        Err(crate::Error::NeedsThreads(work))
    }
}
