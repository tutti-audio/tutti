//! The endpoint device watcher against the service's always-present diagnostic loopback pair.

use std::collections::BTreeSet;
use std::sync::mpsc::channel;

use crate::deadline::Deadline;
use tutti_winmidi2_ffi::{Change, Watcher, endpoints, ensure_service_available};

/// **A started watcher reports every endpoint the service lists as added**, so the listing
/// and the watcher agree on the loopback pair's ids and names.
#[test]
fn a_started_watcher_reports_each_listed_endpoint_as_added() {
    ensure_service_available().expect("the MIDI service is available");
    let listed = endpoints().expect("the service lists its endpoints");
    let mut waiting: BTreeSet<String> = listed
        .iter()
        .filter(|e| e.loopback.is_some())
        .map(|e| e.id.to_lowercase())
        .collect();
    assert_eq!(waiting.len(), 2, "both loopback endpoints in {listed:?}");
    let (tx, changes) = channel();
    let _watcher = Watcher::start(move |change| {
        // The test may have ended: a send to a dropped receiver is the end.
        let _ = tx.send(change);
    })
    .expect("the watcher starts");
    let deadline = Deadline::start();
    while !waiting.is_empty() {
        let change = deadline
            .recv(&changes)
            .unwrap_or_else(|e| panic!("{waiting:?} never added: {e}"))
            .expect("a change that can be read");
        if let Change::Added(endpoint) = change {
            let listed = listed
                .iter()
                .find(|e| e.id.eq_ignore_ascii_case(&endpoint.id));
            if let Some(listed) = listed {
                assert_eq!(&endpoint, listed, "the watcher describes it as the listing");
            }
            waiting.remove(&endpoint.id.to_lowercase());
        }
    }
}
