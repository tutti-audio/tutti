//! The yield that lets the executor look at a task between two slices of blocking work.

/// Returns to the executor once, which sees a dropped task or a shut-down
/// executor between two slices of blocking work.
pub(crate) async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}
