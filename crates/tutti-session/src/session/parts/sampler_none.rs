//! [`SamplerPart`] without the `sampler` feature: no stream is opened. `sampler` is this
//! module's twin when the sampler is built.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_platform::Executor;
use tutti_rt::Bells;
use tutti_units::SampleRate;

use crate::SessionFloat;

/// No disk streamer runs.
#[derive(Debug)]
pub(crate) struct SamplerPart<F: SessionFloat>(std::marker::PhantomData<F>);

impl<F: SessionFloat> SamplerPart<F> {
    pub(crate) fn new(_: bool, _: SampleRate, _: &Executor, _: &Bells) -> crate::Result<Self> {
        Ok(Self(std::marker::PhantomData))
    }

    /// No streamer to wake.
    pub(crate) fn wake(&self) {}

    /// No stream has a ratio.
    pub(crate) fn set_sample_rate(&self, _: SampleRate) {}
}
