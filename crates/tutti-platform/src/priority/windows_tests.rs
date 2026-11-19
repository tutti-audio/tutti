use super::*;

/// Windows: `Raised` is `ABOVE_NORMAL`, `Highest` is `HIGHEST`. Compiled
/// by `cargo xtask check-windows`, run only on Windows.
#[test]
fn the_requests_map_to_above_normal_and_highest() {
    use thread_priority::WinAPIThreadPriority;
    let win = |r| WinAPIThreadPriority::try_from(wanted(r)).unwrap();
    assert_eq!(
        win(PriorityRequest::Raised),
        WinAPIThreadPriority::AboveNormal
    );
    assert_eq!(win(PriorityRequest::Highest), WinAPIThreadPriority::Highest);
}
