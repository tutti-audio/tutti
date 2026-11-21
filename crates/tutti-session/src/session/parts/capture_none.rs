//! The capture inputs without the `capture` feature: none is added. `input::Inputs` is this
//! module's twin when capture is built.

use tutti_graph::NodeKey;

use crate::SessionFloat;
use crate::builder::Options;

/// No capture input.
#[derive(Debug)]
pub(crate) struct Inputs<F: SessionFloat>(std::marker::PhantomData<F>);

impl<F: SessionFloat> Inputs<F> {
    pub(crate) fn from_options(_: &mut Options) -> Self {
        Self(std::marker::PhantomData)
    }

    /// No take to report.
    pub(crate) fn land_takes(&mut self, _: &mut Vec<crate::SessionEvent>) {}

    /// No input to forget.
    pub(crate) fn forget(&mut self, _: NodeKey) {}
}
