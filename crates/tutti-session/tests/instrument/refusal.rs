//! An effect plugin is not an instrument: `Session::instrument` refuses it,
//! naming the role the plugin declared. (The reference probe declares itself an
//! effect; the shared plugin's descriptor lists both.)

use crate::instrument_support::subjects::Plugin;
use tutti_plugin_types::PluginRole;
use tutti_session::{Error, NotInstrument};

#[test]
fn an_effect_plugin_is_refused() {
    let (mut session, node) = Plugin::effect();
    match session.instrument(node) {
        Err(Error::NotInstrument {
            why: NotInstrument::Plugin(PluginRole::Effect),
            ..
        }) => {}
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("an effect plugin was accepted"),
    }
}
