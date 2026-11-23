//! A table test's row runner.

/// Runs `check` over every labelled row of a table test. A failing row does not stop the
/// others, and the failure names every failing row with its panic message.
///
/// # Panics
///
/// If any row panics, after every row has run.
#[track_caller]
pub fn for_each_row<T>(rows: impl IntoIterator<Item = (&'static str, T)>, check: impl Fn(T)) {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut failed = Vec::new();
    for (label, row) in rows {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| check(row))) {
            let why = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("a non-text panic");
            failed.push(format!("{label}: {why}"));
        }
    }
    assert!(failed.is_empty(), "failing rows:\n{}", failed.join("\n"));
}
