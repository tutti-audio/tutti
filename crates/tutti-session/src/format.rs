//! [`Sealed`]: the steps of an update that only an `f32` session has.
//!
//! A private module with a public trait, so [`SessionFloat`](crate::SessionFloat)
//! names it and no other crate can implement it.

#[cfg(feature = "device")]
use crate::DeviceClock;
use crate::{AppKey, Clock, PhaseCx, Session, SessionEvent};

/// A point in a phase where an `f32` session has work an `f64` one does not.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// Modulation applied.
    Modulation,
    /// The declarations re-applied after the commit.
    Reapply,
    /// The plugins served.
    Plugins,
}

/// The steps of an update that depend on the session's float format being
/// `f32`: the phases that serve modulation and plugins.
pub trait Sealed: tutti_rt::RingFloat {
    /// The `f32`-only part of `step` of a phase: all of it on an `f32` session,
    /// none on an `f64` one.
    #[doc(hidden)]
    fn host<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        step: Step,
        cx: PhaseCx<'_>,
        out: &mut Vec<SessionEvent>,
    );

    /// Lands what a hosted plugin's job ended with: only an `f32` session
    /// hosts plugins, so an `f64` one submits no plugin job.
    #[doc(hidden)]
    #[cfg(feature = "plugin")]
    fn land_plugin<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        landing: crate::jobs::PluginLanding<'_>,
        out: &mut Vec<SessionEvent>,
    );

    /// Swaps the pooled plugins into their nodes: only an `f32` session
    /// hosts plugins.
    #[doc(hidden)]
    #[cfg(feature = "plugin")]
    fn swap_pooled_plugins<K: AppKey, C: Clock<Float = Self>>(session: &mut Session<K, C>);

    /// The event that lands an opened stream: [`Opened`](crate::SamplerEvent::Opened)
    /// for an `f32` session, [`OpenedF64`](crate::SamplerEvent::OpenedF64) for an `f64` one.
    #[doc(hidden)]
    #[cfg(feature = "sampler")]
    fn opened(
        id: crate::SamplerLoad,
        stream: tutti_sampler::Stream<Self>,
        reader: tutti_sampler::StreamReader<Self>,
    ) -> crate::SamplerEvent;

    /// Announces the render mode to the hosted plugins: only an `f32` session
    /// hosts plugins, so an `f64` one has nobody to tell.
    #[doc(hidden)]
    fn announce_render_mode<K: AppKey, C: Clock<Float = Self>>(session: &mut Session<K, C>);

    /// Reopens the capture inputs after a device restart: the inputs are
    /// `f32` streams, so an `f32` session has them and an `f64` one has none.
    #[doc(hidden)]
    #[cfg(feature = "device")]
    fn reopen_inputs<K: AppKey>(session: &mut Session<K, DeviceClock<Self>>)
    where
        Self: crate::SessionFloat;
}

impl Sealed for f32 {
    fn host<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        step: Step,
        cx: PhaseCx<'_>,
        out: &mut Vec<SessionEvent>,
    ) {
        session.host_step(step, cx, out);
    }

    #[cfg(feature = "plugin")]
    fn land_plugin<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        landing: crate::jobs::PluginLanding<'_>,
        out: &mut Vec<SessionEvent>,
    ) {
        session.land_plugin_job(landing, out);
    }

    #[cfg(feature = "plugin")]
    fn swap_pooled_plugins<K: AppKey, C: Clock<Float = f32>>(session: &mut Session<K, C>) {
        session.swap_pooled_plugins();
    }

    #[cfg(feature = "sampler")]
    fn opened(
        id: crate::SamplerLoad,
        stream: tutti_sampler::Stream<f32>,
        reader: tutti_sampler::StreamReader<f32>,
    ) -> crate::SamplerEvent {
        crate::SamplerEvent::Opened { id, stream, reader }
    }

    fn announce_render_mode<K: AppKey, C: Clock<Float = f32>>(session: &mut Session<K, C>) {
        #[cfg(feature = "plugin")]
        session.render_mode_phase();
        #[cfg(not(feature = "plugin"))]
        let _ = session;
    }

    #[cfg(feature = "device")]
    fn reopen_inputs<K: AppKey>(session: &mut Session<K, DeviceClock<f32>>) {
        #[cfg(feature = "audio-io")]
        session.reopen_inputs();
        #[cfg(not(feature = "audio-io"))]
        let _ = session;
    }
}

impl Sealed for f64 {
    fn host<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        _: Step,
        _: PhaseCx<'_>,
        _: &mut Vec<SessionEvent>,
    ) {
    }

    #[cfg(feature = "plugin")]
    fn land_plugin<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        _: crate::jobs::PluginLanding<'_>,
        _: &mut Vec<SessionEvent>,
    ) {
    }

    #[cfg(feature = "plugin")]
    fn swap_pooled_plugins<K: AppKey, C: Clock<Float = f64>>(_: &mut Session<K, C>) {}

    #[cfg(feature = "sampler")]
    fn opened(
        id: crate::SamplerLoad,
        stream: tutti_sampler::Stream<f64>,
        reader: tutti_sampler::StreamReader<f64>,
    ) -> crate::SamplerEvent {
        crate::SamplerEvent::OpenedF64 { id, stream, reader }
    }

    fn announce_render_mode<K: AppKey, C: Clock<Float = f64>>(_: &mut Session<K, C>) {}

    #[cfg(feature = "device")]
    fn reopen_inputs<K: AppKey>(_: &mut Session<K, DeviceClock<f64>>) {}
}
