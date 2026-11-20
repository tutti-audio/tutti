//! The executor a test's served plugins watch their servers on.

use std::sync::OnceLock;
use tutti_platform::Executor;

/// One executor for the whole test process.
///
/// A guard keeps only a weak handle to its executor, and an executor that
/// drops stops the exit watches it runs, so a test passes this one (which lives
/// for the process) and not a temporary.
pub fn executor() -> &'static Executor {
    static EXECUTOR: OnceLock<Executor> = OnceLock::new();
    EXECUTOR.get_or_init(Executor::new)
}
