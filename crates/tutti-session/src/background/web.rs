//! The web: a page starts no thread, so the browser's own queue drives the executor.

use std::io;

use tutti_platform::{Executor, PageDriver};

/// No thread can start.
pub(crate) const THREADS: bool = false;

/// The handle of the future that drives the executor from the browser's queue; dropping it
/// stops the driver.
#[must_use = "dropping it stops the driver"]
pub(crate) struct Running {
    _driver: PageDriver,
}

/// Hands the browser's queue the one future that fires `executor`'s sleeps and runs its
/// tasks. A blocking closure runs on the page's thread inside it, for as long as it takes.
/// A build that needs a thread fails first ([`require`](super::require)).
#[expect(clippy::unnecessary_wraps, reason = "the native twin's signature")]
pub(crate) fn start(executor: &Executor) -> io::Result<Running> {
    Ok(Running {
        _driver: executor.drive_on_page(),
    })
}
