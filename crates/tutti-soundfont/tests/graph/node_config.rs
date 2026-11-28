//! A unit's config is a `NodeConfig`: it lists no value (the unit has no param a host sets
//! while it plays), and a change of any field is a new node.

use std::sync::Arc;

use tutti_graph::{Initials, IntoNode, NodeConfig};
use tutti_midi_protocol::SysexDeviceId;
use tutti_soundfont::{Polyphony, SoundFontConfig};
use tutti_ump::MidiGroup;

use crate::support::font::one_preset;

/// One config's row: `base`, and the variants that each change one structure field.
///
/// Checks (a) the initials are none, which the bound pairs with a node whose controls are
/// `()`; (b) a clone is the same structure both ways; (c) each structure variant is not.
fn check<C>(name: &str, base: &C, structures: &[C])
where
    C: NodeConfig<f32> + Clone,
    C::Node: IntoNode<f32, Controls = ()>,
{
    assert_eq!(base.initials(), Initials::new(), "{name}: no param");
    let same = base.clone();
    assert!(
        C::same_structure(base, &same) && C::same_structure(&same, base),
        "{name}: a clone is the same structure"
    );
    for (k, other) in structures.iter().enumerate() {
        assert!(
            !C::same_structure(base, other) && !C::same_structure(other, base),
            "{name}: structure variant {k} is a new node"
        );
    }
}

#[test]
fn a_soundfont_config_lists_no_param_and_compares_its_structure() {
    let font = one_preset(vec![vec![]]);
    let base = SoundFontConfig::new(Arc::clone(&font)).expect("a playable font");
    check(
        "soundfont",
        &base,
        &[
            base.clone().polyphony(Polyphony::of(8)),
            base.clone().group(MidiGroup::of(1)),
            base.clone().device(SysexDeviceId::of(5)),
            // The same contents behind another handle is another font.
            SoundFontConfig::new(one_preset(vec![vec![]])).expect("a playable font"),
        ],
    );
}
