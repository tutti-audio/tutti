//! How the Initiator's outcome of a Property Exchange call reaches the app.

use tutti_midi_ci::{
    BodyLimit, PaginationFault, PeOutcome, PeReply, PeRequestId, PeResponse, PeSignal, PeStatus,
    PeTerminator, PeUnreadable, ResourceFailure,
};

use std::sync::Arc;

use tutti_midi_ci::Muid;
use tutti_midi_ci::{
    PeData, PeJson, PeResource, PeResourceList, ResourceGet, ResourceTarget, SubscribeId,
};
use tutti_midi_device::DeviceId;
use tutti_midi_hardware::test_support::property_peer;
use tutti_midi_hardware::{PeerId, PropertyCall};
use tutti_ump::MidiGroup;

use super::{
    Ended, GetProperty, PeFailure, PeerError, Pending, PortLink, PropertySlot, Reply, Slot,
    Subscribe, Subscription,
};
use crate::job::{JobOutputQueue, Ticket};
use crate::peer::ResourceIds;

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

/// M2-103 section 12.1: a NAK of a failure other than a terminate is the peer's
/// signal, unchanged.
#[test]
fn a_nak_that_does_not_terminate_lands_as_its_signal() {
    let signal = PeSignal::OutOfSequence {
        request_id: PeRequestId::of(5),
    };
    assert_eq!(
        PeFailure::of(&ResourceFailure::Peer(Box::new(PeOutcome::Refused(signal)))),
        PeerError::Refused(PeFailure::Nak(signal))
    );
}

/// A reply or Property Data that cannot be used lands as unreadable, with the
/// failure that says why, whichever layer found it.
#[test]
fn an_unusable_reply_or_body_lands_as_unreadable_with_its_failure() {
    let failures = [
        ResourceFailure::Peer(Box::new(PeOutcome::Unreadable(PeUnreadable::Abandoned))),
        ResourceFailure::Pagination(PaginationFault::NoTotalCount),
    ];
    for failure in failures {
        assert_eq!(
            PeFailure::of(&failure),
            PeerError::Refused(PeFailure::Unreadable(failure.clone()))
        );
    }
}

/// A body that is not JSON, and JSON that is not the Resource's data, land as
/// unreadable with the failure that names which.
#[test]
fn a_body_that_is_not_json_or_not_the_resources_lands_as_unreadable() {
    let Err(json) = PeJson::try_from(b"{".as_slice()) else {
        panic!("not JSON");
    };
    let number = PeJson::try_from(b"5".as_slice()).expect("JSON");
    let Err(data) = PeData::decode(&PeResource::LocalOn, &number) else {
        panic!("not LocalOn's data");
    };
    for failure in [ResourceFailure::Json(json), ResourceFailure::Data(data)] {
        assert_eq!(
            PeFailure::of(&failure),
            PeerError::Refused(PeFailure::Unreadable(failure.clone()))
        );
    }
}

/// The id of a fake device.
fn device_id() -> DeviceId {
    use tutti_midi_device::PortDirection;
    use tutti_midi_hardware::test_support::{FakeBackend, endpoint};
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let inlets = tutti_midi_runtime::InputInlets::new(
        tutti_graph::EventCapacity::of(64),
        std::time::Duration::ZERO,
    );
    let session = tutti_midi_hardware::MidiSession::open_over(
        backend,
        inlets,
        tutti_midi_hardware::MidiHardwareConfig::new(),
    )
    .expect("opens over the fake OS");
    fake.plug(endpoint(7, "Wire"), PortDirection::Duplex);
    session.outputs().first().expect("the device").id.clone()
}

/// A Pending that ends as the session going (its port dropped) lands every call it
/// holds, in flight or waiting, as `Disconnected`.
#[test]
fn a_pending_that_ends_as_disconnected_lands_its_property_calls_disconnected() {
    let queue: JobOutputQueue = Arc::default();
    let peer = property_peer(
        device_id(),
        MidiGroup::FIRST,
        Muid::of(0x0012_3456),
        PeResourceList::new(),
    );
    let ask = peer.ask(PropertyCall::Get(ResourceGet::new(
        ResourceTarget::try_from(PeResource::LocalOn).unwrap(),
    )));
    let id = PeerId {
        group: MidiGroup::FIRST,
        muid: Muid::of(0x0012_3456),
    };
    let mut pending = Pending::new(ResourceIds::new());
    let mut jobs = Vec::new();
    for sent in [true, false] {
        let (job, ticket) = Ticket::<GetProperty>::new(&queue);
        jobs.push(job);
        pending.properties.push(PropertySlot::Get(Slot {
            peer: id,
            request: ask.clone(),
            sent,
            tickets: vec![Reply::Job(ticket)],
        }));
    }
    pending.end(Ended::Disconnected);
    let mut outputs: Vec<_> = queue.lock().unwrap().drain(..).collect();
    for job in &jobs {
        let result = outputs.iter_mut().find_map(|l| l.take(job));
        assert_eq!(result, Some(Err(PeerError::Disconnected)));
    }
}

/// A lease that is still live when its job is dropped ends its Subscription as it
/// drops, which lands a job on the same queue: the drop of the job discards it with
/// the queue's lock released. A lock held across the discard hangs this test.
#[test]
fn dropping_a_job_whose_landed_lease_is_live_does_not_deadlock() {
    let queue: JobOutputQueue = Arc::default();
    let link = PortLink::new(std::sync::Weak::new(), Arc::clone(&queue));
    let peer = property_peer(
        device_id(),
        MidiGroup::FIRST,
        Muid::of(0x0012_3456),
        PeResourceList::new(),
    );
    let ask = peer.ask(PropertyCall::Subscribe(ResourceGet::new(
        ResourceTarget::try_from(PeResource::LocalOn).unwrap(),
    )));
    let (job, ticket) = Ticket::<Subscribe>::new(&queue);
    let (lease, _held) = Subscription::new(
        ResourceIds::new().mint(),
        link,
        &ask,
        PeResource::LocalOn,
        SubscribeId::new("s1").expect("an id"),
    );
    ticket.land(Ok(lease));
    assert_eq!(queue.lock().unwrap().len(), 1, "the lease is in the queue");
    drop(job);
    assert!(queue.lock().unwrap().is_empty());
}

/// A peer's id is minted once per Group and MUID: asking again, also after the
/// peer left and came back, gives the same id, and another MUID has its own.
#[test]
fn a_peer_keeps_its_id_and_another_muid_has_its_own() {
    let pending = Pending::new(ResourceIds::new());
    let at = |muid: u32| PeerId {
        group: MidiGroup::FIRST,
        muid: Muid::of(muid),
    };
    let first = pending.peer_id(at(0x0012_3456));
    assert_eq!(pending.peer_id(at(0x0012_3456)), first);
    let other = pending.peer_id(at(0x0045_6789));
    assert_ne!(other, first);
    assert_eq!(pending.peer_id(at(0x0012_3456)), first);
}

/// A request whose answering end is dropped without an answer resolves to
/// `Disconnected`: the session ended it, and the future never waits for ever.
#[test]
fn a_request_whose_sender_is_dropped_resolves_to_disconnected() {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::Request;

    let (sender, request) = Request::<u8>::new();
    drop(sender);
    let mut request = pin!(request);
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(
        request.as_mut().poll(&mut cx),
        Poll::Ready(Err(PeerError::Disconnected))
    );
}
