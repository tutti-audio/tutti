//! A source's feed follows its kind: a follower is never declared without the node it
//! follows, and a source is never changed to a kind with another feed.

use crate::common::session;
use tutti_graph::NodeKey;
use tutti_modulation::{Follower, Lfo, Macro, Source};
use tutti_session::SourceError;

/// **A follower is declared with its feed or not at all.** `Mods::source(follower)` has no
/// node to follow: it is refused at the call, naming the source, and nothing is added.
#[test]
fn a_follower_declared_without_its_feed_is_refused_at_the_call() {
    let mut s = session();
    let mut m = s.mods_mut();
    assert_eq!(
        m.source(Follower::default()),
        Err(SourceError::NeedsFeed { kind: "follower" })
    );
    assert!(m.view().is_empty(), "nothing was added");
    assert!(m.source(Macro(0.5)).is_ok());
}

/// **A source keeps the kind its feed belongs to.** A follower changed to a macro, and a
/// macro to a follower, is refused and changes nothing; a follower of another time is a
/// same-kind change and keeps its node.
#[test]
fn a_source_changed_to_another_kind_is_refused_and_keeps_its_feed() {
    let mut s = session();
    let mut m = s.mods_mut();
    let follower = m.follower(Follower::default(), NodeKey(7));
    let before = m.view().source(follower).expect("declared");
    assert_eq!(
        m.set_source(follower, Macro(0.5)),
        Err(SourceError::ChangesKind {
            id: follower,
            from: "follower",
            to: "macro",
        })
    );
    assert_eq!(m.view().source(follower), Some(before));

    let knob = m.macro_(0.5);
    assert!(matches!(
        m.set_source(knob, Follower::default()),
        Err(SourceError::ChangesKind { .. })
    ));
    assert!(matches!(
        m.view().source(knob).unwrap().source,
        Source::Macro { .. }
    ));
    assert_eq!(m.view().source(knob).unwrap().fed_by, None);

    let slower = Follower::default().attack(tutti_units::Seconds::new(0.5));
    m.set_source(follower, slower).expect("the same kind");
    assert_eq!(m.view().source(follower).unwrap().fed_by, Some(NodeKey(7)));
    let lfo = m.lfo(Lfo::sine());
    assert!(m.set_source(lfo, Lfo::sine()).is_ok());
}
