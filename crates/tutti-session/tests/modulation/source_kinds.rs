//! A source's feed follows its kind: a source is never changed to a kind with another feed.

use crate::common::{self, session};
use tutti_graph::NodeKey;
use tutti_modulation::{Lfo, Macro, TimeSource};
use tutti_session::{ModSource, SourceError};

/// **A source keeps the kind its feed belongs to.** A follower changed to a macro, and a
/// macro to a follower, is refused and changes nothing; a follower of another time is a
/// same-kind change and keeps its node.
#[test]
fn a_source_changed_to_another_kind_is_refused_and_keeps_its_feed() {
    let mut s = session();
    let mut m = s.mods_mut();
    let follower = m.follower(common::meter(), NodeKey(7));
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
        m.set_source(knob, common::meter()),
        Err(SourceError::ChangesKind { .. })
    ));
    assert!(matches!(
        m.view().source(knob).unwrap().source,
        ModSource::Time(TimeSource::Macro { .. })
    ));
    assert_eq!(m.view().source(knob).unwrap().fed_by, None);

    let slower =
        tutti_units::Ballistics::new(tutti_units::Seconds::new(0.5), common::meter().release())
            .expect("valid times");
    m.set_source(follower, slower).expect("the same kind");
    assert_eq!(m.view().source(follower).unwrap().fed_by, Some(NodeKey(7)));
    let lfo = m.lfo(Lfo::sine());
    assert!(m.set_source(lfo, Lfo::sine()).is_ok());
}
