//! The web's default executor: futures run on the caller's `poll`, since a page
//! starts no threads of its own.

use super::Executor;

pub(super) fn default_executor() -> Executor {
    Executor::polled()
}
