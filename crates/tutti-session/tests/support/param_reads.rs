//! A plugin's parameter list and values, read the way a host's mirror reads them
//! (`PluginParamValues`): the public path, not the plugin handle's raw door.

use tutti_plugin_host::{PluginHandle, PluginParamValues};
use tutti_units::Normalized;
use tutti_units::ParamId;

/// What a test reads of a plugin through its handle.
pub(crate) trait ParamReads {
    /// The plugin's value for `id`: `None` when the list is unavailable or the plugin
    /// does not list `id`.
    fn parameter_value(&self, id: ParamId) -> Option<Normalized>;
}

fn mirror(handle: &PluginHandle) -> Option<PluginParamValues> {
    let (mut values, read) = PluginParamValues::start(handle);
    values.land(futures_lite::future::block_on(std::future::IntoFuture::into_future(read)).ok()?);
    Some(values)
}

impl ParamReads for PluginHandle {
    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        mirror(self)?.get(id)?
    }
}
