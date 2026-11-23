//! What a plugin says of itself at load reaches the app in its info: the names it
//! gives to MIDI keys and the groups it sorts its parameters into, with each
//! parameter's group path in the listing. The shared test plugin (CLAP, in a real
//! `plugin-server`) declares the pads Kick and Snare and the module path
//! `Voices/Oscillator/Filter` for `aux`.
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`); the
//! tests fail without it, by design.

use crate::loading::{LoadPlugin, Spec};
use tutti_buffers::MaxBlock;
use tutti_plugin_types::{NoteName, ParamGroup, PluginRef};
use tutti_session::{Peer as _, Session};
use tutti_ump::NoteNumber;
use tutti_units::{ParamId, Samples};

/// The shared plugin's `aux`, which sits in the `Filter` module.
const AUX: ParamId = ParamId::new(1);

/// **A plugin's note names and parameter groups are in its info, as it declared
/// them, and a parameter's group is the path of one of them.**
#[test]
fn a_loaded_plugins_info_lists_its_note_names_and_param_groups() {
    tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let mut s = Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds");
    let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let file = tutti_fixtures::publish_with_extension(&real, "clap");
    let (plugin, node) = s
        .load_plugin_handle(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(file)),
        )
        .expect("the shared plugin loads");
    let info = plugin.info();

    let pad = |key, text: &str| NoteName {
        key: NoteNumber::of(key),
        channel: None,
        text: text.into(),
    };
    assert_eq!(info.note_names(), [pad(36, "Kick"), pad(38, "Snare")]);

    let group = |path: &str, label: &str, parent: Option<&str>| ParamGroup {
        path: path.into(),
        label: label.into(),
        parent: parent.map(Into::into),
    };
    assert_eq!(
        info.param_groups(),
        [
            group("Voices", "Voices", None),
            group("Voices/Oscillator", "Oscillator", Some("Voices")),
            group(
                "Voices/Oscillator/Filter",
                "Filter",
                Some("Voices/Oscillator")
            ),
        ]
    );

    let _ = s.update();
    let view = s.plugin_view(node).expect("hosted");
    let aux = view.params().get(AUX).expect("the plugin lists aux");
    assert_eq!(aux.info().group(), "Voices/Oscillator/Filter");
}
