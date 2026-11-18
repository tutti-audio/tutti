//! [`Sealed`]: the steps of an update that depend on the session's float format.
//!
//! A private module with a public trait, so [`SessionFloat`](crate::SessionFloat)
//! names it and no other crate can implement it.

#[cfg(feature = "device")]
use crate::DeviceClock;
use crate::{AppKey, Clock, PhaseCx, Session, SessionEvent};

/// The steps of an update that depend on the session's float format: the
/// ones that serve plugins (`f32` at their FFI) and land a sampler stream.
pub trait Sealed: tutti_rt::RingFloat {
    /// Serves the hosted plugins in `Phase::Plugins`: only an `f32` session
    /// hosts plugins, so an `f64` one has nobody to serve.
    #[doc(hidden)]
    fn serve_plugins<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
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
    fn serve_plugins<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        cx: PhaseCx<'_>,
        out: &mut Vec<SessionEvent>,
    ) {
        #[cfg(feature = "plugin")]
        session.serve_plugins(cx, out);
        #[cfg(not(feature = "plugin"))]
        let _ = (session, cx, out);
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
    fn serve_plugins<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
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

    fn announce_render_mode<K: AppKey, C: Clock<Float = f64>>(_: &mut Session<K, C>) {}

    #[cfg(feature = "device")]
    fn reopen_inputs<K: AppKey>(_: &mut Session<K, DeviceClock<f64>>) {}
}
