//! The Get cache of the Initiator against a scripted peer: the Property Data of
//! a reply is reused for its `cacheTime` (M2-103 §7.4.3), request by request.

use std::num::NonZeroU32;
use std::time::Duration;

use super::super::super::chunk::PeRequestId;
use super::super::super::flow::PeMessageKind;
use super::super::super::header::{PeCacheTime, PePage, PeReply, PeStatus, PeUpdateCommand};
use super::super::super::tree::PeJson;
use super::super::exchange::{
    Script, entry_target, get, paginating_list, program, program_page, sent_gets,
    subscribed_script, target, update,
};
use super::super::{
    CanSet, PeData, PeLocalOn, PeProgramList, PeResource, PeResourceList, ResourceEntry,
    ResourceEvent, ResourceGet, ResourceSet,
};

fn page(offset: u32) -> ResourceGet {
    ResourceGet::new(entry_target(PeResource::ProgramList, "g")).paged(PePage {
        offset,
        limit: NonZeroU32::new(3).unwrap(),
    })
}

/// The reply of the peer to request `id`, carrying `cache_time` seconds when given.
fn answer(script: &mut Script, id: PeRequestId, cache_time: Option<u32>, body: &PeJson) {
    let mut header = PeReply::new(PeStatus::Ok);
    header.cache_time = cache_time.map(PeCacheTime::from_secs);
    script.reply_with(id, PeMessageKind::GET_REPLY, header, body.encode());
}

/// Gets `request` at `at` seconds after the script began, and answers it with
/// `cache_time` when it was sent. Returns whether it was sent.
fn get_at(
    script: &mut Script,
    request: &ResourceGet,
    at: Duration,
    cache_time: Option<u32>,
) -> bool {
    let sent = sent_gets(script);
    script.now = script.start() + at;
    let (_, actions) = script.init.get(request, script.now).unwrap();
    script.take(actions);
    let went = sent_gets(script) > sent;
    if went {
        let id = script.last_get_id();
        answer(script, id, cache_time, &program_page(0, 3));
    } else {
        assert!(
            matches!(
                script.events.last(),
                Some(ResourceEvent::Got { result: Ok(_), .. })
            ),
            "{:?}",
            script.events
        );
    }
    went
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// M2-103 §7.4.3: during its cacheTime the Initiator uses the data it got, and
// asks again only after; an instant at the deadline is past it.
/// Spec: m2-103/7.4.3#2, m2-103/7.3#4
#[test]
fn a_get_within_the_cache_time_is_answered_from_the_cache() {
    let mut script = Script::new(&paginating_list());
    assert!(get_at(&mut script, &page(0), secs(0), Some(30)));
    script.events.clear();
    let before = Duration::from_millis(29_999);
    assert!(!get_at(&mut script, &page(0), before, Some(30)));
    let want = PeData::ProgramList(PeProgramList::new((0..3).map(program).collect()));
    assert!(
        matches!(script.events.as_slice(), [ResourceEvent::Got { result: Ok(data), .. }] if *data == want),
        "{:?}",
        script.events
    );
    assert!(get_at(&mut script, &page(0), secs(30), Some(30)));
}

// M2-103 §7.4.3: a reply that carries no cacheTime, or 0, is held for no time.
/// Spec: m2-103/7.4.3#2
#[test]
fn a_reply_without_a_cache_time_or_with_zero_is_not_cached() {
    let mut script = Script::new(&paginating_list());
    assert!(get_at(&mut script, &page(0), secs(0), None));
    assert!(get_at(&mut script, &page(0), secs(0), Some(0)));
}

// M2-103 §7.4.3: each request has its own cacheTime: one page at 30 seconds and
// the next at 60 expire 30 and 60 seconds after their replies.
/// Spec: m2-103/7.4.3#3, m2-103/7.3#4
#[test]
fn pages_expire_each_on_its_own_cache_time() {
    let mut script = Script::new(&paginating_list());
    assert!(get_at(&mut script, &page(0), secs(0), Some(30)));
    assert!(get_at(&mut script, &page(3), secs(0), Some(60)));
    // Another page is another request.
    assert!(get_at(&mut script, &page(6), secs(1), None));
    assert!(!get_at(&mut script, &page(0), secs(29), Some(30)));
    assert!(!get_at(&mut script, &page(3), secs(29), Some(60)));
    // The first expires, and its new reply starts its own time.
    assert!(get_at(&mut script, &page(0), secs(30), Some(30)));
    assert!(!get_at(&mut script, &page(3), secs(59), Some(60)));
    assert!(!get_at(&mut script, &page(0), secs(59), Some(30)));
    assert!(get_at(&mut script, &page(3), secs(60), Some(60)));
    assert!(get_at(&mut script, &page(0), secs(60), Some(30)));
}

// M2-103 §7.4.3: a notification of a subscribed Resource overrides the cacheTime.
#[test]
fn a_subscription_update_overrides_the_cache_time() {
    let (mut script, id) = subscribed_script();
    let read = get(PeResource::LocalOn);
    let sent = sent_gets(&script);
    let (_, actions) = script.init.get(&read, script.now).unwrap();
    script.take(actions);
    let request = script.last_get_id();
    let mut header = PeReply::new(PeStatus::Ok);
    header.cache_time = Some(PeCacheTime::from_secs(100));
    script.reply_with(request, PeMessageKind::GET_REPLY, header, b"false".to_vec());
    assert_eq!(sent_gets(&script), sent + 1);
    let (_, actions) = script.init.get(&read, script.now).unwrap();
    script.take(actions);
    assert_eq!(sent_gets(&script), sent + 1, "held while nothing changed");

    script.subscription(5, update(PeUpdateCommand::Full, &id), b"true");
    let (_, actions) = script.init.get(&read, script.now).unwrap();
    script.take(actions);
    assert_eq!(sent_gets(&script), sent + 2, "the update dropped the cache");
}

// A Set changes the data of the Resource, so what an earlier Get held is no longer
// it; a Set that failed changed nothing.
#[test]
fn a_set_drops_what_a_get_held() {
    let list = PeResourceList::new().with(
        ResourceEntry::builder(PeResource::LocalOn)
            .can_set(CanSet::Full)
            .build()
            .unwrap(),
    );
    let mut script = Script::new(&list);
    let read = get(PeResource::LocalOn);
    let ask = |script: &mut Script| {
        let sent = sent_gets(script);
        let (_, actions) = script.init.get(&read, script.now).unwrap();
        script.take(actions);
        let went = sent_gets(script) > sent;
        if went {
            let id = script.last_get_id();
            let mut header = PeReply::new(PeStatus::Ok);
            header.cache_time = Some(PeCacheTime::from_secs(100));
            script.reply_with(id, PeMessageKind::GET_REPLY, header, b"false".to_vec());
        }
        went
    };
    let set = |script: &mut Script, status: PeStatus| {
        let new = ResourceSet::full(
            target(PeResource::LocalOn),
            PeData::LocalOn(PeLocalOn(true)),
        )
        .unwrap();
        let (_, actions) = script.init.set(&new, script.now).unwrap();
        script.take(actions);
        let id = script.last_request_id();
        script.reply_with(
            id,
            PeMessageKind::SET_REPLY,
            PeReply::new(status),
            Vec::new(),
        );
    };
    assert!(ask(&mut script));
    assert!(!ask(&mut script), "held");
    set(&mut script, PeStatus::BadRequest);
    assert!(!ask(&mut script), "a failed Set changed nothing");
    set(&mut script, PeStatus::Ok);
    assert!(ask(&mut script), "the Set dropped the data held");
}

// A Get in flight when the Subscription updates its Resource got data the update
// has replaced, so its reply is not held.
#[test]
fn a_reply_overtaken_by_a_subscription_update_is_not_cached() {
    let (mut script, id) = subscribed_script();
    let read = get(PeResource::LocalOn);
    let sent = sent_gets(&script);
    let (_, actions) = script.init.get(&read, script.now).unwrap();
    script.take(actions);
    let request = script.last_get_id();
    script.subscription(5, update(PeUpdateCommand::Full, &id), b"true");
    let mut header = PeReply::new(PeStatus::Ok);
    header.cache_time = Some(PeCacheTime::from_secs(100));
    script.reply_with(request, PeMessageKind::GET_REPLY, header, b"false".to_vec());
    let (_, actions) = script.init.get(&read, script.now).unwrap();
    script.take(actions);
    assert_eq!(sent_gets(&script), sent + 2);
}
