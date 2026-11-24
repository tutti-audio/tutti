//! What a Responder does beyond a plain reply, and what carries more than JSON:
//! the queue of Subscription messages, Flow Control, Timeout Wait, media types
//! and Link actions. Each test names the property of the spec it holds.

use std::time::Duration;

use super::super::capabilities::{PeCiMessage, SimultaneousRequests};
use super::super::chunk::{PeChunk, PeRequestId, split_message};
use super::super::flow::{
    PE_TIMEOUT, PeAnswer, PeMessageKind, PeSignal, PeTerminator, PeTransaction,
};
use super::super::header::{
    PeEncoding, PeEndedBy, PeHeader, PeReply, PeSetScope, PeStatus, PeSubscription, PeUpdateCommand,
};
use super::super::initiator::{PeCall, PeSet};
use super::super::limits::{MediaType, ResourceName};
use super::super::partial::{JsonPointer, PartialError, PartialUpdate, PartialValue};
use super::super::reassemble::PeFlowControl;
use super::super::tree::{PeJson, PeObject};
use super::super::value::{PeExtra, PeExtraError};
use super::exchange::*;
use super::*;
use crate::testing::{OURS, PEER};
use crate::{CiHeader, CiMessage, Decoded, Initiator, MaxSysexSize, Muid, TimeoutWait};

fn from_initiator(message: CiMessage) -> Decoded {
    Decoded::Message(message)
}

/// A message from the Initiator to the Responder.
fn pe_message(message: PeCiMessage) -> Decoded {
    from_initiator(CiMessage::PropertyExchange {
        header: CiHeader::new(Muid::of(OURS)),
        to: Muid::of(PEER),
        message,
    })
}

/// A signal of the Initiator about a transaction of the Responder's.
fn signal_message(transaction: PeTransaction, signal: PeSignal) -> Decoded {
    let header = CiHeader::new(Muid::of(OURS));
    let to = Muid::of(PEER);
    from_initiator(match signal.answer(transaction) {
        PeAnswer::Ack(ack) => CiMessage::Ack { header, to, ack },
        PeAnswer::Nak(nak) => CiMessage::Nak { header, to, nak },
    })
}

/// An inquiry from the Initiator, all its chunks, and what the Responder did.
fn inquire(
    link: &mut Link,
    kind: PeMessageKind,
    id: u8,
    header: PeHeader,
    body: &[u8],
) -> Vec<ResponderAction> {
    let chunks = split_message(PeRequestId::of(id), &header, body, MaxSysexSize::of(512)).unwrap();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(link.resp.on_message(
            &mut link.store,
            &pe_message(PeCiMessage::Chunk { kind, chunk }),
            link.now,
        ));
    }
    out
}

/// A reply of the Initiator to a Subscription message of the Responder's.
fn reply_to_update(link: &mut Link, id: PeRequestId, status: PeStatus) -> Vec<ResponderAction> {
    let chunks = split_message(
        id,
        &PeHeader::Reply(PeReply::new(status)),
        &[],
        MaxSysexSize::of(256),
    )
    .unwrap();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(link.resp.on_message(
            &mut link.store,
            &pe_message(PeCiMessage::Chunk {
                kind: PeMessageKind::SUBSCRIPTION_REPLY,
                chunk,
            }),
            link.now,
        ));
    }
    out
}

fn chunks_of(actions: &[ResponderAction]) -> Vec<(PeMessageKind, PeChunk)> {
    actions
        .iter()
        .filter_map(|a| match a {
            ResponderAction::Send(CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            }) => Some((*kind, chunk.clone())),
            _ => None,
        })
        .collect()
}

fn header_of(chunk: &PeChunk) -> PeHeader {
    PeHeader::try_from(chunk.header()).unwrap()
}

fn acks_and_naks(actions: &[ResponderAction]) -> Vec<PeSignal> {
    actions
        .iter()
        .filter_map(|a| match a {
            ResponderAction::Send(CiMessage::Ack { ack, .. }) => {
                PeSignal::from_ack(ack).map(|(_, s)| s)
            }
            ResponderAction::Send(CiMessage::Nak { nak, .. }) => {
                PeSignal::from_nak(nak).map(|(_, s)| s)
            }
            _ => None,
        })
        .collect()
}

/// The Subscription messages among the actions: header and Property Data.
fn updates_of(actions: &[ResponderAction]) -> Vec<(PeSubscription, Vec<u8>)> {
    chunks_of(actions)
        .into_iter()
        .filter(|(kind, _)| *kind == PeMessageKind::SUBSCRIPTION_INQUIRY)
        .filter_map(|(_, chunk)| match header_of(&chunk) {
            PeHeader::Subscription(s) => Some((s, chunk.data().to_vec())),
            _ => None,
        })
        .collect()
}

fn channels(n: u16) -> PeData {
    PeData::ChannelList(PeChannelList::new(vec![channel(n, "Changing")]))
}

fn subscribed_link(
    requests: u8,
    resource: impl Into<PeResource>,
    res_id: Option<&str>,
) -> (Link, ResourceTarget) {
    let resource = resource.into();
    let mut link = Link::taking(store(), 256, requests);
    let encoding = if resource == custom("X-Blob") {
        PeEncoding::Mcoded7
    } else {
        PeEncoding::Ascii
    };
    let target = match res_id {
        Some(i) => entry_target(resource, i),
        None => super::exchange::target(resource),
    };
    let made = link.init.subscribe(
        &ResourceGet::new(target.clone()).encoded(encoding),
        link.now,
    );
    link.call(made);
    assert_eq!(link.resp.subscriptions().count(), 1);
    (link, target)
}

fn request_id(actions: &[ResponderAction]) -> PeRequestId {
    chunks_of(actions)[0].1.request_id()
}

// M2-103 §5.3, §11.1: a Subscription message waits for a Request ID when the
// Initiator takes one at a time, goes when the reply to the one before arrives,
// and carries the data as it is then.
/// Spec: m2-103/11#9
#[test]
fn an_update_waits_for_a_free_request_id_and_carries_the_data_as_it_is_then() {
    let (mut link, target) = subscribed_link(1, StandardResource::ChannelList, None);
    let first = link
        .resp
        .publish(&mut link.store, &target, channels(1), link.now)
        .unwrap();
    assert_eq!(chunks_of(&first).len(), 1);
    let second = link
        .resp
        .publish(&mut link.store, &target, channels(2), link.now)
        .unwrap();
    let third = link
        .resp
        .publish(&mut link.store, &target, channels(3), link.now)
        .unwrap();
    assert!(second.is_empty() && third.is_empty());
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    assert_eq!(updates.len(), 1);
    let (PeSubscription::Update { command, .. }, body) = &updates[0] else {
        panic!("not an update");
    };
    assert_eq!(*command, PeUpdateCommand::Full);
    assert_eq!(
        PeJson::try_from(body.as_slice()).unwrap(),
        channels(3).to_json().unwrap(),
        "the newest data goes, and the two before it with it"
    );
    assert_eq!(link.resp.queued(Muid::of(OURS)), 0);
}

// M2-103 §5.3: the queue never grows past one item for a Subscription, and nothing is
// dropped.
#[test]
fn the_queue_holds_one_item_for_each_subscription() {
    let (mut link, target) = subscribed_link(1, StandardResource::ChannelList, None);
    for n in 1..=20 {
        link.resp
            .publish(&mut link.store, &target, channels(n), link.now)
            .unwrap();
    }
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
}

fn pointer(text: &str) -> JsonPointer {
    JsonPointer::new(text).unwrap()
}

// M2-103 §11.1: `partial` updates use the format of a Partial Set; two that wait
// together are one, the later value of a pointer winning.
#[test]
fn queued_partial_updates_fold_into_one() {
    let (mut link, target) = subscribed_link(1, custom("X-ProgramEdit"), Some("abcd"));
    let change = |p: &str, n: i64| PartialUpdate::new().with(pointer(p), PartialValue::Integer(n));
    let first = link
        .resp
        .change(&mut link.store, &target, change("/lfoSpeed", 10), link.now)
        .unwrap();
    link.resp
        .change(
            &mut link.store,
            &target,
            change("/pitchEnvelope/rates/0", 80),
            link.now,
        )
        .unwrap();
    link.resp
        .change(
            &mut link.store,
            &target,
            change("/pitchEnvelope/levels/0", 60),
            link.now,
        )
        .unwrap();
    link.resp
        .change(
            &mut link.store,
            &target,
            change("/pitchEnvelope/rates/0", 81),
            link.now,
        )
        .unwrap();
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    let (PeSubscription::Update { command, .. }, body) = &updates[0] else {
        panic!("not an update");
    };
    assert_eq!(*command, PeUpdateCommand::Partial);
    assert_eq!(
        PeJson::try_from(body.as_slice()).unwrap(),
        PeJson::try_from(
            br#"{"/pitchEnvelope/rates/0":81,"/pitchEnvelope/levels/0":60}"#.as_slice()
        )
        .unwrap()
    );
}

// M2-103 §11.1: a `partial` that cannot be one change, a pointer inside another or
// too many, is told as a `full` of the data as it is.
#[test]
fn partial_updates_that_cannot_fold_are_told_as_a_full() {
    let one = |p: &str| PartialUpdate::new().with(pointer(p), PartialValue::Integer(1));
    assert!(super::server::merged(&one("/a"), &one("/b")).is_some());
    assert!(super::server::merged(&one("/a"), &one("/a")).is_some());
    assert!(super::server::merged(&one(""), &one("/a")).is_none());
    assert!(super::server::merged(&one("/a/b"), &one("/a")).is_none());
    let (mut link, target) = subscribed_link(1, custom("X-ProgramEdit"), Some("abcd"));
    let first = link
        .resp
        .change(&mut link.store, &target, one("/lfoSpeed"), link.now)
        .unwrap();
    for n in 0..70 {
        link.resp
            .change(&mut link.store, &target, one(&format!("/p{n}")), link.now)
            .unwrap();
    }
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    let (PeSubscription::Update { command, .. }, body) = &updates[0] else {
        panic!("not an update");
    };
    // The data as it is: a `full` when it fits one chunk, a `notify` when not.
    match command {
        PeUpdateCommand::Full => {
            let held = link.store.get(&target).unwrap().to_json().unwrap();
            assert_eq!(PeJson::try_from(body.as_slice()).unwrap(), held);
        }
        other => assert_eq!(*other, PeUpdateCommand::Notify),
    }
    assert_ne!(*command, PeUpdateCommand::Partial);
}

// M2-103 §11.5: the End of a Subscription is told even when updates wait; it takes
// their place.
#[test]
fn an_end_goes_after_the_replies_and_replaces_queued_updates() {
    let (mut link, target) = subscribed_link(1, StandardResource::ChannelList, None);
    let first = link
        .resp
        .publish(&mut link.store, &target, channels(1), link.now)
        .unwrap();
    link.resp
        .publish(&mut link.store, &target, channels(2), link.now)
        .unwrap();
    let id = link.resp.subscriptions().next().unwrap().0.clone();
    let ended = link.resp.end_subscription(&mut link.store, &id, link.now);
    assert!(chunks_of(&ended).is_empty(), "no Request ID is free");
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    assert!(matches!(
        updates.as_slice(),
        [(
            PeSubscription::End {
                ended_by: Some(PeEndedBy::Responder),
                ..
            },
            _
        )]
    ));
}

// M2-103 §12.2: a Subscription message nobody replied to is given up after the
// timeout, which frees its Request ID for what waits.
#[test]
fn a_timeout_frees_the_request_id_for_the_queue() {
    let (mut link, target) = subscribed_link(1, StandardResource::ChannelList, None);
    link.resp
        .publish(&mut link.store, &target, channels(1), link.now)
        .unwrap();
    link.resp
        .publish(&mut link.store, &target, channels(2), link.now)
        .unwrap();
    assert!(link.resp.deadline().is_some());
    let later = link.now + PE_TIMEOUT + Duration::from_millis(1);
    let sent = link.resp.on_timeout(&mut link.store, later);
    assert_eq!(updates_of(&sent).len(), 1);
    assert_eq!(link.resp.queued(Muid::of(OURS)), 0);
}

// M2-103 §12.2.1: a Timeout Wait ACK for a Subscription message adds its period to
// the time the Responder waits for the reply.
#[test]
fn a_timeout_wait_extends_the_wait_for_a_reply_to_an_update() {
    let (mut link, target) = subscribed_link(1, StandardResource::ChannelList, None);
    let sent = link
        .resp
        .publish(&mut link.store, &target, channels(1), link.now)
        .unwrap();
    let before = link.resp.deadline().unwrap();
    let signal = PeSignal::TimeoutWait {
        request_id: request_id(&sent),
        wait: TimeoutWait::of(30),
        chunk: None,
    };
    link.resp.on_message(
        &mut link.store,
        &signal_message(PeTransaction::Subscription, signal),
        link.now,
    );
    assert_eq!(
        link.resp.deadline().unwrap(),
        before + Duration::from_secs(3)
    );
}

// M2-103 §11.2: a Terminate NAK of a Subscription message frees its Request ID.
#[test]
fn a_terminate_of_an_update_frees_its_request_id() {
    let (mut link, target) = subscribed_link(1, StandardResource::ChannelList, None);
    let sent = link
        .resp
        .publish(&mut link.store, &target, channels(1), link.now)
        .unwrap();
    link.resp
        .publish(&mut link.store, &target, channels(2), link.now)
        .unwrap();
    let signal = PeSignal::Terminate {
        request_id: request_id(&sent),
        by: PeTerminator::Initiator,
    };
    let after = link.resp.on_message(
        &mut link.store,
        &signal_message(PeTransaction::Subscription, signal),
        link.now,
    );
    assert_eq!(updates_of(&after).len(), 1);
}

// M2-103 §5.3, M2-101 §8.5: a peer takes at most 127 Subscription messages at
// once, each under a distinct Request ID of the 128; the update after them waits
// in the queue, and goes when a reply frees an ID.
#[test]
fn the_update_past_the_peers_count_waits_for_a_reply_and_then_goes() {
    let most = SimultaneousRequests::MAX;
    let (mut link, target) = subscribed_link(most, StandardResource::ChannelList, None);
    for _ in 1..=most {
        let made = link.init.subscribe(
            &ResourceGet::new(target.clone()).encoded(PeEncoding::Ascii),
            link.now,
        );
        link.call(made);
    }
    assert_eq!(link.resp.subscriptions().count(), usize::from(most) + 1);
    let sent = link
        .resp
        .publish(&mut link.store, &target, channels(1), link.now)
        .unwrap();
    let mut ids: Vec<u8> = chunks_of(&sent)
        .iter()
        .map(|(_, c)| c.request_id().get())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), usize::from(most), "127 distinct Request IDs");
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1, "the 128th waits");
    let after = reply_to_update(&mut link, request_id(&sent), PeStatus::Ok);
    assert_eq!(updates_of(&after).len(), 1, "it goes when one completes");
    assert_eq!(link.resp.queued(Muid::of(OURS)), 0);
}

fn flow_get(resource: &str) -> PeHeader {
    let mut request = super::super::header::PeRequest::new(ResourceName::new(resource).unwrap());
    request.flow_control = PeFlowControl::On;
    PeHeader::Request(request)
}

/// A link whose ChannelList is long enough for several chunks.
fn long_channel_link() -> Link {
    let mut link = Link::new(store(), 128);
    let many: Vec<PeChannelEntry> = (1..=12)
        .map(|n| channel(n, &format!("Channel number {n}")))
        .collect();
    link.resp
        .publish(
            &mut link.store,
            &super::exchange::target(StandardResource::ChannelList),
            PeData::ChannelList(PeChannelList::new(many)),
            link.now,
        )
        .unwrap();
    link
}

// M2-103 §15.2: under Flow Control the Responder sends a chunk, waits for the ACK that
// asks for the next, and the last chunk needs its ACK too.
/// Spec: m2-103/15.2#1, m2-101/8.8#1
#[test]
fn a_flow_controlled_reply_goes_a_chunk_for_each_ack() {
    let mut link = long_channel_link();
    let mark = link.wire.len();
    let made = link.init.get(
        &get(StandardResource::ChannelList).flow_controlled(),
        link.now,
    );
    let call = link.call(made);
    let Ok(PeData::ChannelList(list)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    assert_eq!(list.entries().len(), 12);
    // After the inquiry the wire alternates: a chunk, its ACK, a chunk, its ACK.
    let flow: Vec<bool> = link.wire[mark + 1..]
        .iter()
        .map(|(to_resp, _)| *to_resp)
        .collect();
    assert!(flow.len() >= 6, "several chunks: {flow:?}");
    for (at, to_resp) in flow.iter().enumerate() {
        assert_eq!(*to_resp, at % 2 == 1, "{flow:?}");
    }
    assert_eq!(flow.last(), Some(&true), "the last chunk is ACKed");
}

/// The first chunk of a flow-controlled Get of the long ChannelList, and the link.
fn started_stream() -> (Link, PeChunk) {
    let mut link = long_channel_link();
    let sent = inquire(
        &mut link,
        PeMessageKind::GET_INQUIRY,
        9,
        flow_get("ChannelList"),
        &[],
    );
    let chunks = chunks_of(&sent);
    assert_eq!(chunks.len(), 1, "one chunk until it is ACKed");
    (link, chunks[0].1.clone())
}

// M2-103 §15.4: a NAK that asks for the last chunk again gets it again, to three times;
// a fourth ends the transaction (§15.5).
/// Spec: m2-103/15.4#1
#[test]
fn a_resend_nak_gets_the_last_chunk_again() {
    let (mut link, first) = started_stream();
    let resend = || {
        signal_message(
            PeTransaction::Get,
            PeSignal::Resend {
                request_id: PeRequestId::of(9),
                last_good: None,
            },
        )
    };
    for _ in 0..3 {
        let again = link.resp.on_message(&mut link.store, &resend(), link.now);
        let chunks = chunks_of(&again);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, first);
    }
    let end = link.resp.on_message(&mut link.store, &resend(), link.now);
    assert!(chunks_of(&end).is_empty());
    assert!(matches!(
        acks_and_naks(&end).as_slice(),
        [PeSignal::Terminate {
            by: PeTerminator::Responder,
            ..
        }]
    ));
    assert!(link.resp.deadline().is_none());
}

// M2-103 §15.5: a chunk with no ACK in 3 seconds is sent again; after three repeats the
// Responder fails with a Terminate NAK.
/// Spec: m2-103/15.5#2, m2-103/15.5#3
#[test]
fn a_chunk_with_no_ack_is_sent_again_three_times_and_then_terminated() {
    let (mut link, first) = started_stream();
    let mut now = link.now;
    for _ in 0..3 {
        now += PE_TIMEOUT + Duration::from_millis(1);
        let again = link.resp.on_timeout(&mut link.store, now);
        let chunks = chunks_of(&again);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, first);
    }
    now += PE_TIMEOUT + Duration::from_millis(1);
    let end = link.resp.on_timeout(&mut link.store, now);
    assert!(matches!(
        acks_and_naks(&end).as_slice(),
        [PeSignal::Terminate {
            by: PeTerminator::Responder,
            ..
        }]
    ));
    assert!(link.resp.deadline().is_none());
}

// M2-103 §15.1: an ACK asks for the chunk after the one it names; an ACK of an
// earlier chunk is a repeat and sends nothing.
#[test]
fn an_ack_sends_the_next_chunk_and_a_repeat_sends_nothing() {
    let (mut link, _) = started_stream();
    let ack = |n: u16| {
        signal_message(
            PeTransaction::Get,
            PeSignal::SendNext {
                request_id: PeRequestId::of(9),
                chunk: super::super::chunk::ChunkNo::of(n),
            },
        )
    };
    let second = link.resp.on_message(&mut link.store, &ack(1), link.now);
    let chunks = chunks_of(&second);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].1.position(),
        super::super::chunk::ChunkPosition::Number(super::super::chunk::ChunkNo::of(2))
    );
    assert!(chunks_of(&link.resp.on_message(&mut link.store, &ack(1), link.now)).is_empty());
}

// M2-103 §15.3: a Set under Flow Control is ACKed a chunk at a time except the
// last, which the reply to the Set acknowledges.
#[test]
fn a_set_under_flow_control_is_acked_a_chunk_at_a_time() {
    let mut link = Link::new(store(), 192);
    let bytes = blob_bytes(400);
    let target = super::exchange::target(custom("X-BigBlob"));
    let set = ResourceSet::full(target.clone(), blob_data("X-BigBlob", &bytes))
        .unwrap()
        .encoded(PeEncoding::Mcoded7);
    let mark = link.wire.len();
    let made = link.init.set(&set, link.now);
    let call = link.call(made);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert_eq!(
        link.store.get(&target),
        Some(&blob_data("X-BigBlob", &bytes))
    );
    let from_resp = link.wire[mark..].iter().filter(|(to, _)| !*to).count();
    let to_resp = link.wire[mark..].iter().filter(|(to, _)| *to).count();
    // One ACK for each chunk but the last, and the reply.
    assert_eq!(
        to_resp, from_resp,
        "chunks sent: {to_resp}, answers {from_resp}"
    );
    assert!(to_resp >= 4);
}

// M2-103 §15.3, Table 15: a Set that does not use Flow Control on a Resource with
// `fcOnSet` is 407, and one that does on a Resource without it is 406.
/// Spec: m2-103/15.3#2, m2-103/15.3#3, m2-101/8.10#1, m2-103/7.4#5
#[test]
fn a_set_answers_407_without_flow_control_and_406_with_it_unasked() {
    let mut raw = Raw::new();
    let body = blob_bytes(20);
    let set = |name: &str, flow: PeFlowControl| {
        let mut set = PeSet::new(ResourceName::new(name).unwrap(), body.clone());
        set.media_type = Some(octets());
        set.encoding = PeEncoding::Mcoded7;
        set.flow_control = flow;
        PeCall::Set(set)
    };
    assert_eq!(
        raw.status(set("X-BigBlob", PeFlowControl::Off)),
        PeStatus::FlowControlRequired
    );
    assert_eq!(
        raw.status(set("X-Blob", PeFlowControl::On)),
        PeStatus::FlowControlUnsupported
    );
    assert_eq!(raw.status(set("X-Blob", PeFlowControl::Off)), PeStatus::Ok);
    assert_eq!(
        raw.status(set("X-BigBlob", PeFlowControl::On)),
        PeStatus::Ok
    );
}

// M2-103 §12.2.1: a Responder that cannot answer in time sends an ACK that asks the
// Initiator to wait, and answers when it can.
/// Spec: m2-103/12.2.1#1, m2-103/12.2#1
#[test]
fn a_busy_resource_is_answered_with_a_timeout_wait_and_then_the_reply() {
    let mut link = Link::new(store(), 256);
    let device = super::exchange::target(StandardResource::DeviceInfo);
    link.resp.mark_busy(device.clone(), TimeoutWait::of(20));
    let start = link.now;
    let made = link.init.get(&get(StandardResource::DeviceInfo), link.now);
    let call = link.call(made);
    assert!(
        !link
            .events
            .iter()
            .any(|e| matches!(e, ResourceEvent::Got { call: c, .. } if *c == call)),
        "no reply yet"
    );
    assert!(
        link.served
            .iter()
            .any(|e| matches!(e, ResponderEvent::Deferred { .. }))
    );
    // The Initiator's wait is the 3 seconds and the 2 the ACK asked for.
    assert_eq!(
        link.init.deadline(),
        Some(start + PE_TIMEOUT + Duration::from_secs(2))
    );
    let sent = link.resp.ready(&mut link.store, &device, link.now);
    link.publish(sent);
    assert_eq!(link.got(call), Ok(PeData::DeviceInfo(device_info())));
}

// M2-103 §12.2.1: while the Resource stays busy the Responder asks for more time
// before the Initiator's wait is over.
#[test]
fn a_responder_asks_again_while_the_resource_stays_busy() {
    let mut link = Link::new(store(), 256);
    let device = super::exchange::target(StandardResource::DeviceInfo);
    link.resp.mark_busy(device, TimeoutWait::of(20));
    let made = link.init.get(&get(StandardResource::DeviceInfo), link.now);
    link.call(made);
    let again = link
        .resp
        .on_timeout(&mut link.store, link.now + Duration::from_secs(2));
    assert!(matches!(
        acks_and_naks(&again).as_slice(),
        [PeSignal::TimeoutWait { wait, .. }] if wait.tenths() == 20
    ));
    // A Resource that is not busy is answered at once.
    let other = link.init.get(&get(StandardResource::LocalOn), link.now);
    let call = link.call(other);
    assert!(link.got(call).is_ok());
}

// M2-103 §12.2.1: a Set for a busy Resource waits too, and is made when it is ready.
#[test]
fn a_set_for_a_busy_resource_is_held_until_it_is_ready() {
    let mut link = Link::new(store(), 256);
    let local_on = super::exchange::target(StandardResource::LocalOn);
    link.resp.mark_busy(local_on.clone(), TimeoutWait::of(10));
    let set = ResourceSet::full(local_on.clone(), PeData::LocalOn(PeLocalOn(true))).unwrap();
    let made = link.init.set(&set, link.now);
    let call = link.call(made);
    assert_eq!(
        link.store.get(&local_on),
        Some(&PeData::LocalOn(PeLocalOn(false)))
    );
    let sent = link.resp.ready(&mut link.store, &local_on, link.now);
    link.publish(sent);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert_eq!(
        link.store.get(&local_on),
        Some(&PeData::LocalOn(PeLocalOn(true)))
    );
}

fn reply_header(link: &Link, to_initiator: bool) -> Vec<super::super::header::PeReply> {
    link.wire
        .iter()
        .filter(|(to, _)| *to != to_initiator)
        .filter_map(|(_, m)| match m {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { chunk, .. },
                ..
            } => match PeHeader::try_from(chunk.header()) {
                Ok(PeHeader::Reply(r)) => Some(r),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

// M2-103 §6.1.5, §7.5: Property Data that is not JSON comes with its `mediaType` and
// reaches the caller as bytes with that type.
/// Spec: m2-103/6.1.5#1, m2-103/7.1#3, m2-103/7.5#1, m2-103/14.6#2
#[test]
fn binary_property_data_reaches_the_caller_with_its_media_type() {
    let mut link = Link::new(store(), 256);
    let blob = super::exchange::target(custom("X-Blob"));
    let made = link.init.get(
        &ResourceGet::new(blob).encoded(PeEncoding::Mcoded7),
        link.now,
    );
    let call = link.call(made);
    let Ok(PeData::Opaque(opaque)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    let OpaqueBody::Binary(binary) = opaque.body() else {
        panic!("not binary");
    };
    assert_eq!(*binary.media_type(), octets());
    assert_eq!(binary.bytes(), blob_bytes(300));
    let replies = reply_header(&link, true);
    assert!(replies.iter().any(|r| r.media_type == Some(octets())));
}

// M2-103 §7.5: a Set of binary data names its media type in the Header Data.
#[test]
fn a_set_of_binary_data_carries_its_media_type() {
    let mut link = Link::new(store(), 256);
    let blob = super::exchange::target(custom("X-Blob"));
    let new = blob_bytes(90);
    let set = ResourceSet::full(blob.clone(), blob_data("X-Blob", &new))
        .unwrap()
        .encoded(PeEncoding::Mcoded7);
    let made = link.init.set(&set, link.now);
    let call = link.call(made);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert_eq!(link.store.get(&blob), Some(&blob_data("X-Blob", &new)));
    let sets: Vec<_> = link
        .wire
        .iter()
        .filter(|(to, _)| *to)
        .filter_map(|(_, m)| match m {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            } if *kind == PeMessageKind::SET_INQUIRY && !chunk.header().is_empty() => {
                Some(header_of(chunk))
            }
            _ => None,
        })
        .collect();
    let [PeHeader::Request(request)] = sets.as_slice() else {
        panic!("{sets:?}");
    };
    assert_eq!(request.media_type, Some(octets()));
    assert_eq!(request.set_scope, PeSetScope::Whole);
}

// M2-103 §14.2: a media type the entry does not list is refused, by the Initiator
// before it is sent and by the Responder with 415.
#[test]
fn a_media_type_the_entry_does_not_list_is_refused() {
    let mut link = Link::new(store(), 256);
    // JSON data for a Resource whose entry lists only octet-stream.
    let blob = super::exchange::target(custom("X-Blob"));
    let json_data = PeData::decode(custom("X-Blob"), &json(r#"{"a":1}"#)).unwrap();
    let set = ResourceSet::full(blob, json_data)
        .unwrap()
        .encoded(PeEncoding::Mcoded7);
    assert!(matches!(
        link.init.set(&set, link.now),
        Err(CallError::Refused(
            RequestRefused::MediaTypeNotDeclared { .. }
        ))
    ));
    // The Responder: text/plain, and JSON by omission.
    let mut raw = Raw::new();
    let mut plain = PeSet::new(ResourceName::new("X-Blob").unwrap(), b"hello".to_vec());
    plain.media_type = MediaType::new("text/plain");
    plain.encoding = PeEncoding::Mcoded7;
    assert_eq!(raw.status(PeCall::Set(plain)), PeStatus::UnsupportedMedia);
    let mut jsonish = PeSet::new(ResourceName::new("X-Blob").unwrap(), b"{}".to_vec());
    jsonish.encoding = PeEncoding::Mcoded7;
    assert_eq!(raw.status(PeCall::Set(jsonish)), PeStatus::UnsupportedMedia);
}

// M2-103 §7.5: a `full` update of binary data carries the media type, and the
// Initiator holds the bytes.
#[test]
fn a_full_update_of_binary_data_carries_its_media_type() {
    let (mut link, target) = subscribed_link(4, custom("X-Blob"), None);
    let new = blob_bytes(40);
    let actions = link
        .resp
        .publish(
            &mut link.store,
            &target,
            blob_data("X-Blob", &new),
            link.now,
        )
        .unwrap();
    let updates = updates_of(&actions);
    let [(PeSubscription::Update { media_type, .. }, _)] = updates.as_slice() else {
        panic!("{updates:?}");
    };
    assert_eq!(*media_type, Some(octets()));
    link.publish(actions);
    let (_, held, how) = link.updates().pop().unwrap();
    assert_eq!(how, UpdateKind::Full);
    assert_eq!(held, blob_data("X-Blob", &new));
}

// M2-103 §6.1.5: only a Resource without a type holds data that is not JSON; and
// binary data is not of the JSON media type.
#[test]
fn only_an_untyped_resource_holds_binary_data() {
    assert!(PeBinary::new(MediaType::new("application/json").unwrap(), vec![]).is_none());
    // The refusal is of the media type, not of its spelling.
    let shouted = MediaType::new("Application/JSON").unwrap();
    assert!(PeBinary::new(shouted.clone(), vec![]).is_none());
    assert!(PeState::binary(shouted, vec![]).is_none());
    assert!(PeBinary::new(octets(), vec![1, 255]).is_some());
    assert!(PeData::decode_bytes(StandardResource::LocalOn, Some(&octets()), b"x").is_err());
    let json_bytes = PeData::decode_bytes(StandardResource::LocalOn, None, b"true").unwrap();
    assert_eq!(json_bytes, PeData::LocalOn(PeLocalOn(true)));
}

// M2-103 §10: a Partial Set is for JSON; binary data has no pointers.
#[test]
fn a_partial_change_does_not_apply_to_binary_data() {
    let mut link = Link::new(store(), 256);
    let blob = super::exchange::target(custom("X-Blob"));
    let update = PartialUpdate::new().with(pointer("/a"), PartialValue::Integer(1));
    assert!(matches!(
        link.resp.change(&mut link.store, &blob, update, link.now),
        Err(PublishError::Data(_))
    ));
}

// A change to a Resource the store declares but holds no data of names the
// Resource, and so does a Set whose data is another Resource's.
#[test]
fn a_publish_that_cannot_be_made_names_the_resource() {
    let mut link = Link::new(store(), 256);
    let update = PartialUpdate::new().with(pointer("/a"), PartialValue::Integer(1));
    let empty = ResourceTarget::try_from(StandardResource::ProgramList)
        .unwrap_or_else(|_| super::exchange::entry_target(StandardResource::ProgramList, "none"));
    assert_eq!(
        link.resp.change(&mut link.store, &empty, update, link.now),
        Err(PublishError::NoData(StandardResource::ProgramList.into()))
    );
    let local_on = super::exchange::target(StandardResource::LocalOn);
    assert_eq!(
        link.resp.publish(
            &mut link.store,
            &local_on,
            PeData::ExternalSync(PeExternalSync(true)),
            link.now
        ),
        Err(PublishError::DataOfAnotherResource {
            data: StandardResource::ExternalSync.into(),
            target: StandardResource::LocalOn.into(),
        })
    );
}

// M2-103 §13.1.1 Table 69: a ProgramList entry with `simpleAction` Links, each with
// its `propData`.
#[test]
fn a_simple_action_link_reads_table_69() {
    let text = r#"[{"bankPC":[0,1,1],"title":"Piano","links":[
        {"resource":"X-Copy","propData":"0_1_1","role":"simpleAction","title":"copy"},
        {"resource":"X-Paste","propData":"0_1_1","role":"simpleAction","title":"paste",
         "confirmMsg":"Paste over Piano?","confirmType":"yesno","confirmIcon":"warning"}]}]"#;
    let data = PeData::decode(StandardResource::ProgramList, &json(text)).unwrap();
    let PeData::ProgramList(list) = &data else {
        panic!("not ProgramList");
    };
    let links = &list.entries()[0].links;
    let Some(LinkRole::Simple(copy)) = &links[0].role else {
        panic!("not simple");
    };
    assert_eq!(copy.prop_data, PeJson::String("0_1_1".to_owned()));
    assert!(copy.confirmation.is_none());
    let Some(LinkRole::Simple(paste)) = &links[1].role else {
        panic!("not simple");
    };
    let confirmation = paste.confirmation.as_ref().unwrap();
    assert_eq!(confirmation.kind, Some(ConfirmType::YesNo));
    assert_eq!(confirmation.icon, Some(ConfirmIcon::Warning));
    assert_eq!(data.to_json(), Some(json(text)));
}

// M2-103 §13.1.1 Table 70: the button Sets the Resource of the Link with the `propData`.
/// Spec: m2-103/13.1.1#2
#[test]
fn a_simple_action_is_a_set_of_its_prop_data() {
    let link = PeLink {
        role: Some(LinkRole::Simple(SimpleAction {
            prop_data: PeJson::String("0_1_1".to_owned()),
            confirmation: None,
        })),
        ..PeLink::new(custom("X-Copy"))
    };
    let set = ResourceSet::try_from(&link).unwrap();
    assert_eq!(set.target().resource(), &custom("X-Copy"));
    let SetBody::Full(data) = set.body() else {
        panic!("not a full set");
    };
    assert_eq!(data.to_wire(), (None, br#""0_1_1""#.to_vec()));
}

// M2-103 §13.1.1 Table 68: `propData` is required of a `simpleAction`; the question
// of a confirmation needs its message.
#[test]
fn a_simple_action_requires_its_properties() {
    let entry = |link: &str| format!(r#"[{{"title":"T","channel":1,"links":[{link}]}}]"#);
    let good = r#"{"resource":"X-A","role":"simpleAction","propData":1}"#;
    assert!(PeData::decode(StandardResource::ChannelList, &json(&entry(good))).is_ok());
    for bad in [
        r#"{"resource":"X-A","role":"simpleAction"}"#,
        r#"{"resource":"X-A","role":"simpleAction","propData":1,"confirmType":"yesno"}"#,
        r#"{"resource":"X-A","role":"simpleAction","propData":1,"confirmMsg":"?","confirmType":"maybe"}"#,
        r#"{"resource":"X-A","role":"simpleAction","propData":1,"confirmMsg":"?","confirmIcon":"star"}"#,
    ] {
        assert!(
            PeData::decode(StandardResource::ChannelList, &json(&entry(bad))).is_err(),
            "{bad}"
        );
    }
}

fn selector_channels() -> &'static str {
    r#"[{"channel":1,"bankPC":[0,1,120],"title":"Piano","links":[
        {"resource":"X-AudioOutList","resId":"ch1","role":"selectorAction","title":"Stereo Main",
         "selectedProperty":"output","actionResource":"X-Output","actionResId":"ch1"}]}]"#
}

// M2-103 §13.1.2 Table 73: a ChannelList with a `selectorAction` Link.
/// Spec: m2-103/13.1.2#2
#[test]
fn a_selector_action_link_reads_table_73() {
    let data = PeData::decode(StandardResource::ChannelList, &json(selector_channels())).unwrap();
    let PeData::ChannelList(list) = &data else {
        panic!("not ChannelList");
    };
    let link = &list.entries()[0].links[0];
    let Some(LinkRole::Selector(action)) = &link.role else {
        panic!("not a selector");
    };
    assert_eq!(link.resource, custom("X-AudioOutList"));
    assert_eq!(action.action_resource, custom("X-Output"));
    assert_eq!(action.action_res_id, Some(id("ch1")));
    assert_eq!(action.selected_property.as_str(), "output");
    assert_eq!(data.to_json(), Some(json(selector_channels())));
}

// M2-103 §13.1.2 Table 76: the chosen entry's `selectedProperty` is the Property
// Data of a Set of the action Resource.
/// Spec: m2-103/13.1.2#8
#[test]
fn a_selection_is_a_set_of_the_selected_property() {
    let data = PeData::decode(StandardResource::ChannelList, &json(selector_channels())).unwrap();
    let PeData::ChannelList(list) = data else {
        panic!("not ChannelList");
    };
    let link = &list.entries()[0].links[0];
    let chosen: PeObject = match json(r#"{"output":"LA","name":"Left Alt","selected":false}"#) {
        PeJson::Object(o) => o,
        _ => panic!("not an object"),
    };
    let set = ResourceSet::from_selection(link, &chosen).unwrap();
    assert_eq!(set.target().resource(), &custom("X-Output"));
    assert_eq!(set.target().res_id(), Some(&id("ch1")));
    let SetBody::Full(data) = set.body() else {
        panic!("not a full set");
    };
    assert_eq!(data.to_wire(), (None, br#""LA""#.to_vec()));
    let lacking: PeObject = match json(r#"{"name":"Left Alt"}"#) {
        PeJson::Object(o) => o,
        _ => panic!("not an object"),
    };
    assert!(matches!(
        ResourceSet::from_selection(link, &lacking),
        Err(ActionError::NoSelectedProperty(_))
    ));
}

// M2-103 Table 72: `actionResource` and `selectedProperty` are required of a
// `selectorAction`.
#[test]
fn a_selector_action_requires_its_properties() {
    let entry = |link: &str| format!(r#"[{{"title":"T","channel":1,"links":[{link}]}}]"#);
    for bad in [
        r#"{"resource":"X-L","role":"selectorAction","selectedProperty":"output"}"#,
        r#"{"resource":"X-L","role":"selectorAction","actionResource":"X-O"}"#,
        r#"{"resource":"X-L","role":"selectorAction","actionResource":"X-O","selectedProperty":"not a name"}"#,
    ] {
        assert!(
            PeData::decode(StandardResource::ChannelList, &json(&entry(bad))).is_err(),
            "{bad}"
        );
    }
}

// M2-103 §13 Table 64: any other role is kept by its name, of at most 32
// characters, the manufacturer's `x-` roles among them.
/// Spec: m2-103/13#4
#[test]
fn another_role_is_kept_by_name() {
    let entry = |role: &str| {
        format!(r#"[{{"title":"T","channel":1,"links":[{{"resource":"X-A","role":"{role}"}}]}}]"#)
    };
    let text = entry("x-vendorRole");
    let PeData::ChannelList(list) =
        PeData::decode(StandardResource::ChannelList, &json(&text)).unwrap()
    else {
        panic!("not ChannelList");
    };
    assert!(
        matches!(&list.entries()[0].links[0].role, Some(LinkRole::Other(n)) if n.as_str() == "x-vendorRole")
    );
    assert!(
        PeData::decode(
            StandardResource::ChannelList,
            &json(&entry(&"r".repeat(33)))
        )
        .is_err()
    );
}

// M2-103 §7.1.1, §10: one JSON value type reads and writes both: a header property
// and a change of a Partial Set are scalars, Property Data is anything.
#[test]
fn a_header_property_and_a_change_are_scalars_of_the_one_json_type() {
    let mut extra = PeExtra::default();
    let name = super::super::limits::PropertyName::new("x-a").unwrap();
    assert_eq!(
        extra.insert(name.clone(), PeJson::Array(vec![])),
        Err(PeExtraError::NotScalar)
    );
    assert_eq!(
        extra.insert(name.clone(), PeJson::Null),
        Err(PeExtraError::NotScalar)
    );
    assert!(extra.insert(name, PeJson::Integer(1)).is_ok());
    assert_eq!(
        PartialValue::try_from(&PeJson::Object(PeObject::new())),
        Err(PartialError::NotAChange)
    );
    // The grammar of Header Data has no whitespace and no nesting; Property Data has both.
    assert!(PeHeader::try_from(br#"{"resource": "A"}"#.as_slice()).is_err());
    assert!(PeHeader::try_from(br#"{"resource":"A","x-n":[1]}"#.as_slice()).is_err());
    assert!(PeJson::try_from(br#"{"resource": "A", "x-n": [1]}"#.as_slice()).is_ok());
}

fn terminate_message(transaction: PeTransaction, id: u8) -> Decoded {
    signal_message(
        transaction,
        PeSignal::Terminate {
            request_id: PeRequestId::of(id),
            by: PeTerminator::Initiator,
        },
    )
}

/// The chunks of a Set of X-Blob, of three or more chunks, from the Initiator.
fn blob_set_chunks() -> Vec<PeChunk> {
    let mut request = super::super::header::PeRequest::new(ResourceName::new("X-Blob").unwrap());
    request.mutual_encoding = Some(PeEncoding::Mcoded7);
    request.media_type = Some(octets());
    let body = super::super::mcoded7::encode_vec(&blob_bytes(400));
    let chunks = split_message(
        PeRequestId::of(5),
        &PeHeader::Request(request),
        &body,
        MaxSysexSize::of(192),
    )
    .unwrap();
    assert!(chunks.len() >= 3);
    chunks
}

fn send_set_chunks(link: &mut Link, chunks: &[PeChunk]) -> Vec<ResponderAction> {
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(link.resp.on_message(
            &mut link.store,
            &pe_message(PeCiMessage::Chunk {
                kind: PeMessageKind::SET_INQUIRY,
                chunk: chunk.clone(),
            }),
            link.now,
        ));
    }
    out
}

// M2-103 §12.1.2: on a Terminate NAK of a Set, the side that expects the chunks, the
// Responder, stops expecting the rest of the Request Id, and no reply follows: the
// Set is never made.
/// Spec: m2-103/12.1.2#3, m2-103/12.1.2#4
#[test]
fn a_terminate_nak_of_a_set_stops_the_responder_expecting_chunks() {
    let target = super::exchange::target(custom("X-Blob"));
    let chunks = blob_set_chunks();
    let before = store().get(&target).cloned();
    // All the chunks make the Set.
    let mut whole = Link::new(store(), 192);
    send_set_chunks(&mut whole, &chunks);
    assert_ne!(whole.store.get(&target).cloned(), before);
    // The NAK after the first one leaves the data as it was, and nothing is sent.
    let mut link = Link::new(store(), 192);
    send_set_chunks(&mut link, &chunks[..1]);
    let answer = link.resp.on_message(
        &mut link.store,
        &terminate_message(PeTransaction::Set, 5),
        link.now,
    );
    assert!(answer.is_empty(), "a reply to the NAK: {answer:?}");
    let rest = send_set_chunks(&mut link, &chunks[1..]);
    assert!(
        chunks_of(&rest).is_empty(),
        "a reply to a Set that was terminated"
    );
    assert_eq!(link.store.get(&target).cloned(), before);
}

// M2-103 §12.1.2, §12.2.3: on the Initiator's Terminate NAK of a Get, or its NAK that
// a timeout has occurred, the Responder stops sending chunks of that Request Id, and
// answers the NAK with nothing: the Transaction is concluded.
/// Spec: m2-103/12.1.2#2, m2-103/12.1.2#4, m2-103/12.2.3#2
#[test]
fn a_nak_that_ends_a_get_stops_the_chunks_and_gets_no_reply() {
    let next = |n: u16| {
        signal_message(
            PeTransaction::Get,
            PeSignal::SendNext {
                request_id: PeRequestId::of(9),
                chunk: super::super::chunk::ChunkNo::of(n),
            },
        )
    };
    // Without a NAK the ACK of chunk 1 draws chunk 2.
    let (mut link, _) = started_stream();
    assert_eq!(
        chunks_of(&link.resp.on_message(&mut link.store, &next(1), link.now)).len(),
        1
    );
    for signal in [
        PeSignal::Terminate {
            request_id: PeRequestId::of(9),
            by: PeTerminator::Initiator,
        },
        PeSignal::TimedOut {
            request_id: PeRequestId::of(9),
        },
    ] {
        let (mut link, _) = started_stream();
        let answer = link.resp.on_message(
            &mut link.store,
            &signal_message(PeTransaction::Get, signal),
            link.now,
        );
        assert!(answer.is_empty(), "{signal:?} was answered: {answer:?}");
        assert!(link.resp.deadline().is_none(), "{signal:?}: still timed");
        let after = link.resp.on_message(&mut link.store, &next(1), link.now);
        assert!(chunks_of(&after).is_empty(), "{signal:?}");
    }
}

// M2-103 §13.1: an Initiator makes an action only of a role the Link declares: a
// Link with no role, with another role or with the other action's cannot be made
// into the Set of either.
/// Spec: m2-103/13.1#5
#[test]
fn an_action_is_made_only_of_the_role_the_link_declares() {
    let simple = PeLink {
        role: Some(LinkRole::Simple(SimpleAction {
            prop_data: PeJson::Bool(true),
            confirmation: None,
        })),
        ..PeLink::new(StandardResource::LocalOn)
    };
    let PeData::ChannelList(list) =
        PeData::decode(StandardResource::ChannelList, &json(selector_channels())).unwrap()
    else {
        panic!("not ChannelList");
    };
    let selector = list.entries()[0].links[0].clone();
    let chosen = match json(r#"{"output":"LA"}"#) {
        PeJson::Object(o) => o,
        _ => panic!("not an object"),
    };
    assert!(ResourceSet::try_from(&simple).is_ok());
    assert!(ResourceSet::from_selection(&selector, &chosen).is_ok());
    let other = |role| PeLink {
        role,
        ..PeLink::new(custom("X-Copy"))
    };
    let undeclared = [
        other(None),
        other(Some(LinkRole::Cluster)),
        other(Some(LinkRole::Other(
            RoleName::try_from("x-vendorRole").unwrap(),
        ))),
    ];
    for link in undeclared.iter().chain([&selector]) {
        assert_eq!(
            ResourceSet::try_from(link),
            Err(ActionError::NotThatAction("simpleAction")),
            "{link:?}"
        );
    }
    for link in undeclared.iter().chain([&simple]) {
        assert_eq!(
            ResourceSet::from_selection(link, &chosen),
            Err(ActionError::NotThatAction("selectorAction")),
            "{link:?}"
        );
    }
}

// M2-103 §13.1.1, §13.1.2: an Initiator fills the `resId` the Link has, for the
// Resource it Sets, and a Resource that needs one is refused without it.
/// Spec: m2-103/13.1.1#5, m2-103/13.1.2#6
#[test]
fn an_action_sets_the_entry_its_link_names() {
    let action = |res_id: Option<&str>| PeLink {
        res_id: res_id.map(id),
        role: Some(LinkRole::Simple(SimpleAction {
            prop_data: json("[]"),
            confirmation: None,
        })),
        ..PeLink::new(StandardResource::ProgramList)
    };
    // ProgramList has entries, so its Set names one.
    let set = ResourceSet::try_from(&action(Some("general"))).unwrap();
    assert_eq!(set.target().res_id(), Some(&id("general")));
    assert_eq!(
        ResourceSet::try_from(&action(None)),
        Err(ActionError::Refused(RequestRefused::ResIdRequired(
            StandardResource::ProgramList.into()
        )))
    );
    // The selector's action Resource is filled the same way.
    let selector = |action_res_id: Option<&str>| PeLink {
        role: Some(LinkRole::Selector(SelectorAction {
            action_resource: StandardResource::ProgramList.into(),
            action_res_id: action_res_id.map(id),
            selected_property: super::super::limits::PropertyName::new("output").unwrap(),
        })),
        ..PeLink::new(custom("X-AudioOutList"))
    };
    let chosen = match json(r#"{"output":[]}"#) {
        PeJson::Object(o) => o,
        _ => panic!("not an object"),
    };
    let set = ResourceSet::from_selection(&selector(Some("ch1")), &chosen).unwrap();
    assert_eq!(set.target().res_id(), Some(&id("ch1")));
    assert_eq!(
        ResourceSet::from_selection(&selector(None), &chosen),
        Err(ActionError::Refused(RequestRefused::ResIdRequired(
            StandardResource::ProgramList.into()
        )))
    );
    // The list Resource's own `resId` is read from the Link as the Link has it.
    let PeData::ChannelList(list) =
        PeData::decode(StandardResource::ChannelList, &json(selector_channels())).unwrap()
    else {
        panic!("not ChannelList");
    };
    assert_eq!(list.entries()[0].links[0].res_id, Some(id("ch1")));
}

// M2-103 Table 72: a `resId` of a Link, and the `actionResId` of a selector, are 1 to
// 36 characters of letters, digits and underscores, and no other.
/// Spec: m2-103/13.1.2#7
#[test]
fn a_link_res_id_has_only_the_characters_of_a_res_id() {
    let entry = |res_id: &str, action_res_id: &str| {
        format!(
            r#"[{{"title":"T","channel":1,"links":[{{"resource":"X-OptionList","resId":"{res_id}",
            "role":"selectorAction","actionResource":"X-O","actionResId":"{action_res_id}",
            "selectedProperty":"p"}}]}}]"#
        )
    };
    let reads = |text: String| PeData::decode(StandardResource::ChannelList, &json(&text)).is_ok();
    assert!(reads(entry("a_1", "b_2")));
    let too_long = "a".repeat(37);
    for bad in ["", "a b", "a-b", "a/b", "a.b", too_long.as_str()] {
        assert!(!reads(entry(bad, "b_2")), "resId {bad:?}");
        assert!(!reads(entry("a_1", bad)), "actionResId {bad:?}");
    }
    // The resId the Initiator fills into the Set it makes is the Link's, in those
    // characters.
    let text = r#"[{"title":"T","channel":1,"links":[{"resource":"X-OptionList","resId":"a_1",
        "role":"selectorAction","actionResource":"X-ProgramEdit","actionResId":"abcd",
        "selectedProperty":"p"}]}]"#;
    let PeData::ChannelList(list) =
        PeData::decode(StandardResource::ChannelList, &json(text)).unwrap()
    else {
        panic!("not ChannelList");
    };
    let mut chosen = PeObject::new();
    chosen.insert("p", program_edit().to_json().unwrap());
    let set = ResourceSet::from_selection(&list.entries()[0].links[0], &chosen).unwrap();
    let mut link = Link::new(store(), 256);
    let made = link.init.set(&set, link.now);
    link.call(made);
    let sent: Vec<_> = link
        .wire
        .iter()
        .filter_map(|(to_resp, m)| match m {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            } if *to_resp && *kind == PeMessageKind::SET_INQUIRY && !chunk.header().is_empty() => {
                match PeHeader::try_from(chunk.header()) {
                    Ok(PeHeader::Request(request)) => Some(request),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();
    let [request] = &sent[..] else {
        panic!("one Set inquiry, got {sent:?}");
    };
    let res_id = request
        .res_id
        .as_ref()
        .expect("the resId is filled")
        .as_str();
    assert_eq!(res_id, "abcd");
    assert!(
        res_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    );
}
