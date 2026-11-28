//! The parts of a declared instrument on a build without the `midi` feature:
//! no declaration makes an instrument, so no key has parts. `instrument` is this
//! module's twin when the feature is on.

use std::convert::Infallible;

use tutti_buffers::Float;
use tutti_graph::{NodeKey, Shape};
use tutti_units::Amplitude;

use super::Kept;
use super::wire::Resolved;
use crate::patch::Fading;
use crate::{AnyAssign, AppKey, Clock, Session};

/// No instrument has parts.
pub(super) enum Parts {}

impl Parts {
    pub(super) fn strip(&self) -> NodeKey {
        match *self {}
    }

    pub(super) fn nodes(&self) -> [NodeKey; 2] {
        match *self {}
    }

    pub(super) fn set_gain(&self, _: Amplitude) -> AnyAssign {
        match *self {}
    }
}

/// No declaration makes parts.
pub(super) struct NewParts(Infallible);

impl NewParts {
    #[allow(
        clippy::extra_unused_type_parameters,
        reason = "the call names the graph's float, as the `midi` twin's does"
    )]
    pub(super) fn new<F: Float>(_: Amplitude) -> Self {
        unreachable!("a declaration is an instrument only with the `midi` feature")
    }

    pub(super) fn strip(&self) -> NodeKey {
        match self.0 {}
    }

    pub(super) fn shape(&self) -> Shape {
        match self.0 {}
    }

    pub(super) fn audio(&self, _: NodeKey, _: [u16; 2]) -> [Resolved; 2] {
        match self.0 {}
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    pub(super) fn add_parts(
        &mut self,
        _: &mut Kept<K, C::Float>,
        _: K,
        _: NodeKey,
        _: [u16; 2],
        new: NewParts,
    ) {
        match new.0 {}
    }

    pub(super) fn rejoin(&mut self, parts: &mut Parts, _: NodeKey, _: [u16; 2]) {
        match *parts {}
    }

    pub(super) fn take_out_parts(&mut self, parts: &Parts, _: Fading) {
        match *parts {}
    }
}
