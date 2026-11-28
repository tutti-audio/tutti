//! [`ImpulseResponses`]: the impulse responses of a [`ConvolverConfig`](super::ConvolverConfig),
//! owned.

use tutti_buffers::{Mono, Sample};

/// The impulse responses a convolver config holds, copied from the views they were given as.
///
/// A config owns its responses (an [`Shared`](tutti_graph::Shared) of this type), so it is
/// `'static` and a session can keep it. The responses may differ in length. How many a
/// convolver takes, and which path each is, is its [`IrLayout`](super::IrLayout).
#[derive(Clone, Debug)]
pub struct ImpulseResponses<S: Sample> {
    irs: Vec<Vec<S>>,
}

impl<S: Sample> ImpulseResponses<S> {
    /// The responses `irs`, copied, in order.
    #[must_use]
    pub fn new(irs: &[Mono<'_, S>]) -> Self {
        Self {
            irs: irs.iter().map(|ir| ir.samples().to_vec()).collect(),
        }
    }

    /// How many responses there are.
    pub(super) fn len(&self) -> usize {
        self.irs.len()
    }

    /// The responses as views, in order.
    pub(super) fn views(&self) -> Vec<Mono<'_, S>> {
        self.irs.iter().map(|ir| Mono::new(ir)).collect()
    }
}
