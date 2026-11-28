use std::sync::Arc;

use tutti_buffers::MaxBlock;
use tutti_graph::{Editor, Prepare};
use tutti_modulation::{Declarations, SourceKey, TimeSource};
use tutti_units::{Frame, PlayReading, SampleRate, Samples};

use super::{Derived, OwnMods};
use crate::value::ValueTable;

const fn reading(epoch: u32) -> PlayReading {
    PlayReading {
        epoch,
        play: Frame::new(0),
    }
}

/// Applies `time` with the reading of epoch 7, which the editor's own clock
/// (at its first epoch) is not at; whether it was refused, and whether an
/// update that reads epoch 7 and one that reads epoch 8 are stale.
fn applied(time: &[(SourceKey, TimeSource)]) -> (bool, bool, bool) {
    let prepare = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64)));
    let (mut editor, _exec) = Editor::<f32>::new(prepare);
    let own = OwnMods::new(Arc::new(ValueTable::default()));
    let mut derived = Derived::default();
    let decl = Declarations {
        time,
        signals: &[],
        routes: &[],
        targets: &[],
    };
    let applied = derived.apply(
        &decl,
        &own,
        &mut editor,
        &|_| None,
        &|_, _| None,
        reading(7),
    );
    let stale = |epoch| derived.stale(false, false, (editor.revision(), reading(epoch)));
    (applied.is_err(), stale(7), stale(8))
}

/// **A play start between an apply's reading and the next update is not
/// lost**: what the apply left is dated by the reading it compiled with, so an
/// update that reads a later epoch applies again, and one that reads the same
/// does not.
#[test]
fn a_play_start_after_the_applys_reading_is_applied() {
    assert_eq!(applied(&[]), (false, false, true));
}

/// **A refused apply dates what it saw the same way**: the same source twice
/// is refused, once per epoch.
#[test]
fn a_play_start_after_a_refused_applys_reading_is_applied() {
    let twice = (SourceKey(1), TimeSource::Macro { value: 0.0 });
    assert_eq!(applied(&[twice.clone(), twice]), (true, false, true));
}
