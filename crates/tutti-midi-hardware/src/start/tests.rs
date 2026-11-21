use super::*;
use std::task::Waker;

fn poll_with(start: &mut Start) -> Poll<Result<Started, MidiHardwareError>> {
    Pin::new(start).poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn the_default_start_of_a_backend_is_ready_on_the_first_poll() {
    let (backend, _) = crate::test_support::FakeBackend::build();
    assert!(matches!(
        poll_with(&mut backend.start()),
        Poll::Ready(Ok(_))
    ));
}

#[test]
fn a_ready_start_stays_ready() {
    let mut start = Start::ready();
    assert!(matches!(poll_with(&mut start), Poll::Ready(Ok(_))));
    assert!(matches!(poll_with(&mut start), Poll::Ready(Ok(_))));
}
