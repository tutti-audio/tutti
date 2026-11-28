//! [`Sealed`]: the steps of an update that depend on the session's float format.
//!
//! A private module with a public trait, so [`SessionFloat`](crate::SessionFloat)
//! names it and no other crate can implement it.

use crate::{AppKey, Clock, PhaseCx, Session, SessionEvent};

/// The steps of an update that depend on the session's float format: the
/// ones that serve and land the hosted plugins, which are `f32` at their FFI.
pub trait Sealed: tutti_rt::RingFloat {
    /// What a plugin node is declared as in a patch over a graph of this
    /// float: the held plugin for `f32`, an empty type for `f64`.
    #[doc(hidden)]
    #[cfg(feature = "plugin")]
    type DeclaredPlugin: crate::patch::HoldsPlugin + Send;

    /// Serves the hosted plugins in `Phase::Plugins`: only an `f32` session
    /// hosts plugins, so an `f64` one has nobody to serve.
    #[doc(hidden)]
    fn serve_plugins<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        cx: PhaseCx,
        out: &mut Vec<SessionEvent>,
    );

    /// Announces the render mode to the hosted plugins: only an `f32` session
    /// hosts plugins, so an `f64` one has nobody to tell.
    #[doc(hidden)]
    fn announce_render_mode<K: AppKey, C: Clock<Float = Self>>(session: &mut Session<K, C>);

    /// Puts the declared plugin in the session's graph as a node, taken from its
    /// load: an `f64` session's declared plugin is an empty type.
    #[doc(hidden)]
    #[cfg(feature = "plugin")]
    fn install_declared<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        declared: &Self::DeclaredPlugin,
    ) -> crate::Key<crate::AnyPlugin>;

    /// Swaps the plugin at `node` for the declared one, as `install_declared`
    /// takes it.
    ///
    /// # Errors
    ///
    /// Why the graph refused the swap.
    #[doc(hidden)]
    #[cfg(feature = "plugin")]
    fn swap_declared<K: AppKey, C: Clock<Float = Self>>(
        session: &mut Session<K, C>,
        node: tutti_graph::NodeKey,
        declared: &Self::DeclaredPlugin,
    ) -> Result<(), crate::ReplaceRefused>;

    /// A builder's opener, which serves both floats, as the opener of this one.
    #[doc(hidden)]
    #[cfg(feature = "capture")]
    fn upcast_opener(opener: Box<dyn crate::input::Openers>) -> Box<dyn crate::InputOpener<Self>>;
}

impl Sealed for f32 {
    #[cfg(feature = "plugin")]
    type DeclaredPlugin = crate::patch::PluginHeld;

    #[cfg(feature = "capture")]
    fn upcast_opener(opener: Box<dyn crate::input::Openers>) -> Box<dyn crate::InputOpener<f32>> {
        opener
    }

    fn serve_plugins<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        cx: PhaseCx,
        out: &mut Vec<SessionEvent>,
    ) {
        #[cfg(feature = "plugin")]
        session.serve_plugins(cx, out);
        #[cfg(not(feature = "plugin"))]
        let _ = (session, cx, out);
    }

    fn announce_render_mode<K: AppKey, C: Clock<Float = f32>>(session: &mut Session<K, C>) {
        #[cfg(feature = "plugin")]
        session.render_mode_phase();
        #[cfg(not(feature = "plugin"))]
        let _ = session;
    }

    #[cfg(feature = "plugin")]
    fn install_declared<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        declared: &crate::patch::PluginHeld,
    ) -> crate::Key<crate::AnyPlugin> {
        session
            .add_loaded(&declared.loaded)
            .expect("a plugin is installed only while it is in its slot: the plan checked")
    }

    #[cfg(feature = "plugin")]
    fn swap_declared<K: AppKey, C: Clock<Float = f32>>(
        session: &mut Session<K, C>,
        node: tutti_graph::NodeKey,
        declared: &crate::patch::PluginHeld,
    ) -> Result<(), crate::ReplaceRefused> {
        session.swap_loaded(node, &declared.loaded)
    }
}

impl Sealed for f64 {
    #[cfg(feature = "plugin")]
    type DeclaredPlugin = std::convert::Infallible;

    #[cfg(feature = "capture")]
    fn upcast_opener(opener: Box<dyn crate::input::Openers>) -> Box<dyn crate::InputOpener<f64>> {
        opener
    }

    fn serve_plugins<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        _: PhaseCx,
        _: &mut Vec<SessionEvent>,
    ) {
    }

    fn announce_render_mode<K: AppKey, C: Clock<Float = f64>>(_: &mut Session<K, C>) {}

    #[cfg(feature = "plugin")]
    fn install_declared<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        declared: &std::convert::Infallible,
    ) -> crate::Key<crate::AnyPlugin> {
        match *declared {}
    }

    #[cfg(feature = "plugin")]
    fn swap_declared<K: AppKey, C: Clock<Float = f64>>(
        _: &mut Session<K, C>,
        _: tutti_graph::NodeKey,
        declared: &std::convert::Infallible,
    ) -> Result<(), crate::ReplaceRefused> {
        match *declared {}
    }
}
