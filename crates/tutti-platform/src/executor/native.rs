//! A native target's default executor: its own threads.

use super::Executor;

pub(super) fn default_executor() -> Executor {
    Executor::threaded()
}
