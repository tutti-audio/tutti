//! The capture inputs without the `capture` feature: none is added. `input::Inputs` is this
//! module's twin when capture is built.

use tutti_graph::NodeKey;

use crate::SessionFloat;
use tutti_platform::Executor;

use crate::builder::Options;

/// No capture input.
#[derive(Debug)]
pub(crate) struct Inputs<F: SessionFloat>(std::marker::PhantomData<F>);

impl<F: SessionFloat> Inputs<F> {
    pub(crate) fn from_options(_: &mut Options, _: &Executor) -> Self {
        Self(std::marker::PhantomData)
    }

    /// No input to forget.
    pub(crate) fn forget(&mut self, _: NodeKey) {}
}
