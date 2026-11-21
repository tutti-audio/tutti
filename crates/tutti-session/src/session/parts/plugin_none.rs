//! [`PluginPart`] without the `plugin` feature: nothing is hosted, so each step over it does
//! nothing. `plugin` is this module's twin when plugins are built.

use tutti_buffers::Float;
use tutti_platform::MainThreadMarker;

use crate::graph::Graph;

/// No plugin policy is set.
#[derive(Debug, Default)]
pub(crate) struct PluginOptions;

/// No plugin is hosted.
#[derive(Debug)]
pub(crate) struct PluginPart;

/// No work is handed to the main thread.
#[derive(Debug)]
pub(crate) struct EarlyWork;

impl PluginPart {
    pub(crate) fn new(_: &PluginOptions, _: &tutti_platform::Executor) -> (Self, EarlyWork) {
        (Self, EarlyWork)
    }

    /// No work is handed over.
    pub(crate) fn run_main_work(&self, _: Option<MainThreadMarker>) {}

    /// No plugin is retired.
    pub(crate) fn service_retired<F: Float>(&self, _: &mut Graph<F>, _: Option<MainThreadMarker>) {}
}
