use std::collections::HashSet;
use std::sync::Arc;

use super::{Peer, PeerError, Presence, Resource, ResourceFact, ResourceId, ResourceIds, sealed};

/// A token of the crate's kind, as a lane's would be: a peer with a presence.
struct Token {
    id: ResourceId,
    presence: Presence<&'static str>,
}

impl sealed::Sealed for Token {}

impl Resource for Token {
    fn id(&self) -> ResourceId {
        self.id
    }
}

impl Peer for Token {
    type Info = u32;
    type Cause = &'static str;

    fn info(&self) -> Arc<u32> {
        Arc::new(7)
    }

    fn presence(&self) -> Presence<&'static str> {
        self.presence
    }
}

#[test]
fn ids_are_unique_within_a_session_and_start_at_one() {
    let ids = ResourceIds::new();
    let minted: Vec<ResourceId> = (0..1_000).map(|_| ids.mint()).collect();
    assert_eq!(minted[0].to_string(), "resource 1");
    assert_eq!(minted.iter().collect::<HashSet<_>>().len(), minted.len());
    assert!(minted.windows(2).all(|pair| pair[0] < pair[1]), "ascending");
}

#[test]
fn two_sessions_mint_independently() {
    let (a, b) = (ResourceIds::new(), ResourceIds::new());
    assert_eq!(a.mint(), b.mint(), "the id is the session's, not global");
    assert_ne!(a.mint(), a.mint());
}

#[test]
fn a_token_reads_its_id_info_and_presence() {
    let ids = ResourceIds::new();
    let id = ids.mint();
    let token = Token {
        id,
        presence: Presence::Live,
    };
    assert_eq!(token.id(), id);
    assert_eq!(*token.info(), 7);
    assert_eq!(token.presence(), Presence::Live);
}

#[test]
fn only_live_is_live_and_only_a_gone_peer_has_a_cause() {
    let readings: [(Presence<&str>, bool, Option<&str>); 5] = [
        (Presence::Live, true, None),
        (Presence::Stalled, false, None),
        (
            Presence::Recovering {
                attempt: 2,
                cause: "died",
            },
            false,
            Some("died"),
        ),
        (Presence::Lost("unplugged"), false, Some("unplugged")),
        (Presence::Closed, false, None),
    ];
    for (presence, live, cause) in readings {
        assert_eq!(presence.is_live(), live, "{presence:?}");
        assert_eq!(presence.cause().copied(), cause, "{presence:?}");
    }
}

#[test]
fn a_refusal_keeps_the_requests_own_failure() {
    let refused: PeerError<u32> = PeerError::Refused(404);
    assert_eq!(refused, PeerError::Refused(404));
    assert_ne!(refused, PeerError::Disconnected);
    assert_ne!(PeerError::<u32>::TimedOut, PeerError::Disconnected);
    assert_ne!(PeerError::<u32>::Released, PeerError::Disconnected);
    assert_eq!(refused.to_string(), "the peer refused the request: 404");
}

#[test]
fn a_notice_names_which_reading_to_read_again() {
    let facts = [
        ResourceFact::Presence,
        ResourceFact::Info,
        ResourceFact::Editor,
    ];
    assert_eq!(facts.iter().collect::<HashSet<_>>().len(), facts.len());
}

/// The vocabulary crosses threads: ids and notices ride events, a peer's
/// failure rides a reply.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<ResourceId>();
    send_sync::<ResourceFact>();
    send_sync::<Presence<&'static str>>();
    send_sync::<PeerError<String>>();
};
