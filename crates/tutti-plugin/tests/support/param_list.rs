//! A plugin's parameter list, read the way a host's mirror reads it
//! (`PluginParamValues`): the public path, not the plugin handle's raw door.

use tutti_plugin_host::{PluginHandle, PluginParamValues};
use tutti_plugin_types::ParameterInfo;

/// What a test reads of a plugin's list through its handle.
pub(crate) trait ParamList {
    /// The parameters the plugin lists, `None` when the list is unavailable.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>>;
}

impl ParamList for PluginHandle {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        let (mut values, read) = PluginParamValues::start(self);
        values
            .land(futures_lite::future::block_on(std::future::IntoFuture::into_future(read)).ok()?);
        Some(values.descriptors().to_vec())
    }
}
