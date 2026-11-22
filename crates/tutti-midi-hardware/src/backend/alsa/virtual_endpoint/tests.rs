use super::*;

fn call_failed(errno: i32) -> tutti_alsa_ffi::Error {
    tutti_alsa_ffi::Error::Call {
        operation: Operation::Open,
        source: std::io::Error::from_raw_os_error(errno),
    }
}

// A non-blocking client refuses a send to a full subscriber with -EAGAIN.
#[test]
fn a_subscriber_with_no_room_is_full_not_an_error() {
    assert_eq!(delivery(Err(call_failed(11))).unwrap(), Delivery::Full);
}

#[test]
fn a_sent_message_is_sent() {
    assert_eq!(delivery(Ok(())).unwrap(), Delivery::Sent);
}

#[test]
fn any_other_failure_is_the_os_error() {
    // EBADF.
    assert!(matches!(
        delivery(Err(call_failed(9))),
        Err(MidiHardwareError::Os(_))
    ));
}
