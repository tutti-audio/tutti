//! The Get cache of the Initiator against a scripted peer: the Property Data of
//! a reply is reused for its `cacheTime` (M2-103 §7.4.3), request by request, or
//! Resource by Resource.

use std::num::NonZeroU32;
use std::time::Duration;

use super::super::super::chunk::PeRequestId;
use super::super::super::flow::PeMessageKind;
use super::super::super::header::{PeCacheTime, PePage, PeReply, PeStatus, PeUpdateCommand};
use super::super::super::tree::PeJson;
use super::super::exchange::{
    Script, custom, entry_target, get, json, paginating_list, program, program_page, sent_gets,
    subscribed_script, target, update,
};
use super::super::{
    CanSet, PeCacheScope, PeData, PeLocalOn, PeOpaque, PeProgramList, PeResource, PeResourceList,
    ResourceEntry, ResourceEvent, ResourceGet, ResourceSet,
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

/// A reply to the page at `offset` of the ProgramList, six entries long, with
/// `cache_time` seconds when given.
fn answer_page(script: &mut Script, offset: u8, cache_time: Option<u32>) {
    let id = script.last_get_id();
    let mut header = PeReply::new(PeStatus::Ok);
    header.cache_time = cache_time.map(PeCacheTime::from_secs);
    header.total_count = Some(6);
    let body = program_page(offset, offset + 3);
    script.reply_with(id, PeMessageKind::GET_REPLY, header, body.encode());
}

/// Walks the ProgramList at `at` seconds in, answering each page the walk asks for
/// as `asked` says: its offset and the `cacheTime` of its reply. Returns whether
/// the walk ended with the six entries.
fn walk_at(script: &mut Script, at: Duration, asked: &[(u8, Option<u32>)]) -> bool {
    let sent = sent_gets(script);
    script.events.clear();
    script.now = script.start() + at;
    let get = ResourceGet::new(entry_target(PeResource::ProgramList, "g"));
    let (_, actions) = script
        .init
        .get_list(&get, NonZeroU32::new(3).unwrap(), script.now)
        .unwrap();
    script.take(actions);
    for (offset, time) in asked {
        assert!(
            sent_gets(script) > sent,
            "the walk did not ask for {offset}"
        );
        answer_page(script, *offset, *time);
    }
    assert_eq!(sent_gets(script) - sent, asked.len(), "{:?}", script.events);
    let want = PeData::ProgramList(PeProgramList::new((0..6).map(program).collect()));
    matches!(script.events.as_slice(), [ResourceEvent::Got { result: Ok(data), .. }] if *data == want)
}

// M2-103 §7.4.3: each request has its own cacheTime, so each page of a walk is
// held for the time of its own reply, and a page that is held is not asked for.
/// Spec: m2-103/7.4.3#3
#[test]
fn a_walk_asks_only_for_the_pages_whose_cache_time_has_elapsed() {
    let mut script = Script::new(&paginating_list());
    assert!(walk_at(
        &mut script,
        secs(0),
        &[(0, Some(30)), (3, Some(60))]
    ));
    assert!(walk_at(&mut script, secs(29), &[]), "both pages held");
    // The first expires and the second does not: one request, and the walk ends.
    assert!(walk_at(&mut script, secs(30), &[(0, Some(30))]));
    assert!(walk_at(&mut script, secs(59), &[]));
    assert!(walk_at(
        &mut script,
        secs(60),
        &[(0, Some(30)), (3, Some(60))]
    ));
}

// M2-103 §7.4.3: a page without a cacheTime, or with 0, is held for no time.
/// Spec: m2-103/7.4.3#2
#[test]
fn a_walk_page_without_a_cache_time_is_asked_for_again() {
    let mut script = Script::new(&paginating_list());
    assert!(walk_at(&mut script, secs(0), &[(0, Some(30)), (3, None)]));
    assert!(walk_at(&mut script, secs(1), &[(3, Some(0))]));
    assert!(walk_at(&mut script, secs(2), &[(3, None)]));
}

// A page is cached under its request: a Get of the same page and a walk share it.
/// Spec: m2-103/7.4.3#3
#[test]
fn a_page_a_walk_held_answers_a_get_of_the_same_page() {
    let mut script = Script::new(&paginating_list());
    assert!(walk_at(
        &mut script,
        secs(0),
        &[(0, Some(30)), (3, Some(30))]
    ));
    assert!(!get_at(&mut script, &page(3), secs(10), Some(30)));
    // Another limit is another request.
    let other = ResourceGet::new(entry_target(PeResource::ProgramList, "g")).paged(PePage {
        offset: 3,
        limit: NonZeroU32::new(2).unwrap(),
    });
    assert!(get_at(&mut script, &other, secs(10), Some(30)));
}

/// A script on a ProgramList that paginates and is subscribed to, with the
/// subscription's id.
fn subscribed_list() -> (Script, crate::SubscribeId) {
    let list = PeResourceList::new().with(
        ResourceEntry::builder(PeResource::ProgramList)
            .can_subscribe(true)
            .build()
            .unwrap(),
    );
    let mut script = Script::new(&list);
    let target = entry_target(PeResource::ProgramList, "g");
    let (_, actions) = script
        .init
        .subscribe(&ResourceGet::new(target), script.now)
        .unwrap();
    script.take(actions);
    let start = script.last_request_id();
    let mut reply = PeReply::new(PeStatus::Ok);
    let id = crate::SubscribeId::new("s1").unwrap();
    reply.subscribe_id = Some(id.clone());
    script.reply_with(start, PeMessageKind::SUBSCRIPTION_REPLY, reply, Vec::new());
    let seed = script.last_request_id();
    script.reply(seed, PeStatus::Ok, None, &program_page(0, 6));
    (script, id)
}

// M2-103 §7.4.3: a notification of a subscribed Resource overrides the cacheTime,
// of every page of it.
/// Spec: m2-103/7.4.3#3
#[test]
fn a_subscription_update_drops_every_page_a_walk_held() {
    let (mut script, id) = subscribed_list();
    assert!(walk_at(
        &mut script,
        secs(0),
        &[(0, Some(100)), (3, Some(100))]
    ));
    assert!(walk_at(&mut script, secs(1), &[]));
    script.subscription(
        5,
        update(PeUpdateCommand::Full, &id),
        &program_page(0, 6).encode(),
    );
    assert!(walk_at(
        &mut script,
        secs(2),
        &[(0, Some(100)), (3, Some(100))]
    ));
}

// The data of a Set replaces what a Get held, whatever its page. No standard
// List Resource can be set, so the Resource is a custom one.
#[test]
fn a_set_drops_every_page_a_get_held() {
    let resource = custom("X-ScoreList");
    let list = PeResourceList::new().with(
        ResourceEntry::builder(resource.clone())
            .schema(json(r#"{"title":"Score List","type":"array"}"#))
            .can_paginate(true)
            .can_set(CanSet::Full)
            .build()
            .unwrap(),
    );
    let mut script = Script::new(&list);
    let read = |script: &mut Script, offset: u32| {
        let sent = sent_gets(script);
        let get = ResourceGet::new(target(resource.clone())).paged(PePage {
            offset,
            limit: NonZeroU32::new(3).unwrap(),
        });
        let (_, actions) = script.init.get(&get, script.now).unwrap();
        script.take(actions);
        let went = sent_gets(script) > sent;
        if went {
            let id = script.last_get_id();
            let mut header = PeReply::new(PeStatus::Ok);
            header.cache_time = Some(PeCacheTime::from_secs(100));
            script.reply_with(id, PeMessageKind::GET_REPLY, header, b"[1,2,3]".to_vec());
        }
        went
    };
    assert!(read(&mut script, 0));
    assert!(read(&mut script, 3));
    assert!(!read(&mut script, 0) && !read(&mut script, 3), "held");
    let new = ResourceSet::full(
        target(resource.clone()),
        PeData::Opaque(PeOpaque::new(resource.clone(), PeJson::Array(Vec::new())).unwrap()),
    )
    .unwrap();
    let (_, actions) = script.init.set(&new, script.now).unwrap();
    script.take(actions);
    let id = script.last_request_id();
    script.reply_with(
        id,
        PeMessageKind::SET_REPLY,
        PeReply::new(PeStatus::Ok),
        Vec::new(),
    );
    assert!(read(&mut script, 0), "the Set dropped page 0");
    assert!(read(&mut script, 3), "the Set dropped page 3");
}

// M2-103 §7.4.3: with one cacheTime per Resource the oldest is kept, even when a
// page arrives with a renewed one: the second page, at 10 s with 60 s, does not
// outlive the first, at 0 s with 60 s.
/// Spec: m2-103/7.4.3#4, m2-103/7.4.3#5
#[test]
fn per_resource_a_renewed_cache_time_does_not_lengthen_the_oldest() {
    let mut script = Script::caching(&paginating_list(), PeCacheScope::PerResource);
    assert!(get_at(&mut script, &page(0), secs(0), Some(60)));
    assert!(get_at(&mut script, &page(3), secs(10), Some(60)));
    assert!(!get_at(&mut script, &page(0), secs(59), Some(60)));
    assert!(!get_at(&mut script, &page(3), secs(59), Some(60)));
    assert!(get_at(&mut script, &page(3), secs(60), Some(60)));
}

// The same two pages per request: the second lives its own 60 s.
/// Spec: m2-103/7.4.3#3
#[test]
fn per_request_a_later_page_lives_its_own_cache_time() {
    let mut script = Script::new(&paginating_list());
    assert!(get_at(&mut script, &page(0), secs(0), Some(60)));
    assert!(get_at(&mut script, &page(3), secs(10), Some(60)));
    assert!(get_at(&mut script, &page(0), secs(60), Some(60)));
    assert!(!get_at(&mut script, &page(3), secs(69), Some(60)));
}

// With one cacheTime per Resource no page is used past either limit: a page that
// arrives with a shorter time shortens the whole Resource.
/// Spec: m2-103/7.4.3#5
#[test]
fn per_resource_a_shorter_cache_time_shortens_the_pages_held() {
    let mut script = Script::caching(&paginating_list(), PeCacheScope::PerResource);
    assert!(get_at(&mut script, &page(0), secs(0), Some(60)));
    assert!(get_at(&mut script, &page(3), secs(10), Some(20)));
    assert!(!get_at(&mut script, &page(0), secs(29), Some(60)));
    assert!(get_at(&mut script, &page(0), secs(30), Some(60)));
}

// M2-103 §7.4.3: when the oldest cacheTime has expired as a page arrives, all that
// was cached is removed and only the new page, with its cacheTime, is valid.
/// Spec: m2-103/7.4.3#6
#[test]
fn per_resource_a_page_after_the_expiry_drops_all_that_was_cached() {
    let mut script = Script::caching(&paginating_list(), PeCacheScope::PerResource);
    assert!(get_at(&mut script, &page(0), secs(0), Some(100)));
    assert!(get_at(&mut script, &page(3), secs(5), Some(10)));
    // The Resource expired at 15 s. Page 6 arrives at 20 s with 100 s.
    assert!(get_at(&mut script, &page(6), secs(20), Some(100)));
    assert!(
        get_at(&mut script, &page(0), secs(21), Some(100)),
        "dropped"
    );
    assert!(!get_at(&mut script, &page(6), secs(21), Some(100)));
    assert!(!get_at(&mut script, &page(6), secs(119), Some(100)));
    assert!(get_at(&mut script, &page(6), secs(120), Some(100)));
}

// A walk under one cacheTime per Resource takes its pages from one expiry too.
/// Spec: m2-103/7.4.3#5
#[test]
fn per_resource_a_walk_expires_all_its_pages_together() {
    let mut script = Script::caching(&paginating_list(), PeCacheScope::PerResource);
    assert!(walk_at(
        &mut script,
        secs(0),
        &[(0, Some(30)), (3, Some(60))]
    ));
    assert!(walk_at(&mut script, secs(29), &[]));
    assert!(walk_at(
        &mut script,
        secs(30),
        &[(0, Some(30)), (3, Some(60))]
    ));
}
