//! [`SamplerPart`] without the `sampler` feature: no stream is opened. `sampler` is this
//! module's twin when the sampler is built.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_units::SampleRate;

use crate::SessionFloat;

/// No disk streamer runs.
#[derive(Debug)]
pub(crate) struct SamplerPart<F: SessionFloat>(std::marker::PhantomData<F>);

impl<F: SessionFloat> SamplerPart<F> {
    pub(crate) fn new(_: bool, _: SampleRate, _: tutti_platform::ThreadStart) -> Self {
        Self(std::marker::PhantomData)
    }

    /// No streamer was stepped.
    pub(crate) fn fallbacks(&self) -> u64 {
        0
    }

    /// No streamer to step.
    pub(crate) fn step_until_idle(&mut self) {}

    /// No open is pending.
    pub(crate) fn land_opens(&mut self, _: &mut Vec<crate::SessionEvent>) {}

    /// No stream has a ratio.
    pub(crate) fn set_sample_rate(&self, _: SampleRate) {}
}
