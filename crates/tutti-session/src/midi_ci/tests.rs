//! How the Initiator's outcome of a Property Exchange call reaches the app.

use tutti_midi_device::{
    BodyLimit, PeOutcome, PeReply, PeResponse, PeStatus, PeTerminator, ResourceFailure,
};

use super::{PeFailure, PeerError};

fn failed(status: PeStatus) -> ResourceFailure {
    ResourceFailure::Peer(Box::new(PeOutcome::Failed(PeResponse {
        header: PeReply::new(status),
        body: Vec::new(),
    })))
}

/// M2-103 section 7.4.1: a reply of status 404 is the peer's status with its
/// header, whatever the Initiator did around it; no NAK is made of it.
#[test]
fn a_failure_status_keeps_its_code_and_header() {
    let Err(PeerError::Refused(PeFailure::Status(status, header))) =
        Err::<(), _>(PeFailure::of(&failed(PeStatus::NotFound)))
    else {
        panic!("a status");
    };
    assert_eq!((status.code(), header.status.code()), (404, 404));
}

/// M2-103 section 7.4.1, Table 15: 341 to 343 are the statuses to retry, and they
/// land as such, not as failures.
#[test]
fn a_retry_status_lands_as_a_retry() {
    for status in [
        PeStatus::Unavailable,
        PeStatus::BadData,
        PeStatus::TooManyRequests,
    ] {
        let failure = ResourceFailure::Peer(Box::new(PeOutcome::Retry(PeResponse {
            header: PeReply::new(status),
            body: Vec::new(),
        })));
        assert_eq!(
            PeFailure::of(&failure),
            PeerError::Refused(PeFailure::Retry(status)),
            "{}",
            status.code()
        );
    }
}

/// M2-103 section 12.1: a transaction a Terminate NAK ended names which device
/// ended it, and one past its size limit says so; a timeout is the job's own.
#[test]
fn a_terminate_a_too_large_body_and_a_timeout_are_their_own() {
    let of = |outcome| PeFailure::of(&ResourceFailure::Peer(Box::new(outcome)));
    assert_eq!(
        of(PeOutcome::Terminated(PeTerminator::Responder)),
        PeerError::Refused(PeFailure::Terminated(PeTerminator::Responder))
    );
    assert_eq!(
        of(PeOutcome::TooLarge(BodyLimit::of(64))),
        PeerError::Refused(PeFailure::TooLarge(BodyLimit::of(64)))
    );
    assert_eq!(of(PeOutcome::TimedOut), PeerError::TimedOut);
}
