//! [`Sealed`]: the steps of an update that depend on the session's float format.
//!
//! A private module with a public trait, so [`SessionFloat`](crate::SessionFloat)
//! names it and no other crate can implement it.

use crate::{AppKey, Clock, PhaseCx, Session, SessionEvent};

/// The steps of an update that depend on the session's float format: the
/// ones that serve and land the hosted plugins, which are `f32` at their FFI.
pub trait Sealed: tutti_rt::RingFloat {
    /// Serves the hosted plugins in `Phase::Plugins`: only an `f32` session
    /// hosts plugins, so an `f64` one has nobody to serve.
    #[doc(hidden)]
    fn serve_plugins<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        cx: PhaseCx<'_>,
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

    /// A builder's opener, which serves both floats, as the opener of this one.
    #[doc(hidden)]
    #[cfg(feature = "capture")]
    fn upcast_opener(opener: Box<dyn crate::input::Openers>) -> Box<dyn crate::InputOpener<Self>>;
}

impl Sealed for f32 {
    #[cfg(feature = "capture")]
    fn upcast_opener(opener: Box<dyn crate::input::Openers>) -> Box<dyn crate::InputOpener<f32>> {
        opener
    }

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
    fn swap_pooled_plugins<K: AppKey, C: Clock<Float = f32>>(session: &mut Session<K, C>) {
        session.swap_pooled_plugins();
    }

    fn announce_render_mode<K: AppKey, C: Clock<Float = f32>>(session: &mut Session<K, C>) {
        #[cfg(feature = "plugin")]
        session.render_mode_phase();
        #[cfg(not(feature = "plugin"))]
        let _ = session;
    }
}

impl Sealed for f64 {
    #[cfg(feature = "capture")]
    fn upcast_opener(opener: Box<dyn crate::input::Openers>) -> Box<dyn crate::InputOpener<f64>> {
        opener
    }

    fn serve_plugins<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        _: PhaseCx<'_>,
        _: &mut Vec<SessionEvent>,
    ) {
    }

    #[cfg(feature = "plugin")]
    fn swap_pooled_plugins<K: AppKey, C: Clock<Float = f64>>(_: &mut Session<K, C>) {}

    fn announce_render_mode<K: AppKey, C: Clock<Float = f64>>(_: &mut Session<K, C>) {}
}
